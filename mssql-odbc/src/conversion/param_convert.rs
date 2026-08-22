// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Conversion from a bound application parameter buffer (`BoundParam`) to a
//! TDS RPC parameter (`RpcParameter`).
//!
//! Which C/SQL pairings reach this module is decided at bind time by
//! [`crate::api::type_rules`] and [`crate::params::conversion_matrix`];
//! `SQL_C_DEFAULT` has already been resolved to a concrete C type by then.
//! Data-at-execution is rejected with `HYC00`, `SQL_DEFAULT_PARAM` with
//! `07S01`, and an invalid negative `StrLen_or_Ind` with `HY090`.
//!
//! A `SQL_NULL_DATA` parameter that was bound with `SQL_C_DEFAULT` is
//! materialised as a typed TDS NULL from `sql_type`, because a defaulted
//! binding describes its value through the SQL type rather than through the
//! resolved C type.

use std::slice;

use mssql_tds::datatypes::sql_string::{EncodingType, SqlString};
use mssql_tds::datatypes::sqldatatypes::VectorBaseType;
use mssql_tds::datatypes::sqltypes::SqlType;
use mssql_tds::message::parameters::rpc_parameters::{
    RpcParameter, RpcTypeMetadata, StatusFlags, StreamedSqlType,
};

use crate::api::odbc_types::{
    SQL_BIGINT, SQL_BINARY, SQL_BIT, SQL_C_BINARY, SQL_C_CHAR, SQL_C_WCHAR, SQL_CHAR,
    SQL_DATA_AT_EXEC, SQL_DECIMAL, SQL_DEFAULT_PARAM, SQL_DOUBLE, SQL_FLOAT, SQL_GUID, SQL_INTEGER,
    SQL_LEN_DATA_AT_EXEC_OFFSET, SQL_LONGVARBINARY, SQL_LONGVARCHAR, SQL_NTS, SQL_NULL_DATA,
    SQL_NUMERIC, SQL_REAL, SQL_SMALLINT, SQL_SS_TIME2, SQL_SS_TIMESTAMPOFFSET, SQL_SS_VARIANT,
    SQL_SS_VECTOR, SQL_SS_VECTOR_ELEMENT_SIZE, SQL_SS_XML, SQL_TINYINT, SQL_TYPE_DATE,
    SQL_TYPE_TIME, SQL_TYPE_TIMESTAMP, SQL_VARBINARY, SQL_VARCHAR, SQL_WCHAR, SQL_WLONGVARCHAR,
    SQL_WVARCHAR, SqlLen, SqlSmallInt, SqlSsVectorLayout,
};
use crate::api::sqlstate::{
    DiagMsg, ERR_DATA_AT_EXEC_NOT_IMPLEMENTED, ERR_INVALID_PARAM_COLUMN_SIZE,
    ERR_INVALID_PARAM_DECIMAL_DIGITS, ERR_INVALID_STRING_OR_BUFFER_LENGTH,
    ERR_INVALID_USE_OF_DEFAULT_PARAM, ERR_PARAM_C_TYPE_NOT_IMPLEMENTED,
    ERR_PARAM_SQL_TYPE_NOT_IMPLEMENTED,
};
use crate::params::BoundParam;

/// Why a bound parameter could not be turned into an RPC parameter.
///
/// Each variant carries its own SQLSTATE through [`diag`]; none of these are
/// value-conversion failures, which arrive from
/// [`crate::conversion::error::ConvError`] instead.
///
/// [`diag`]: ParamBuildError::diag
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParamBuildError {
    /// Backstop only: bind time rejects any C type the conversion matrix does
    /// not list, so reaching this means the matrix and this module disagree.
    UnsupportedCType(SqlSmallInt),
    /// The parameter uses data-at-execution (`SQLPutData`).
    DataAtExecUnsupported,
    /// `StrLen_or_Ind` was `SQL_DEFAULT_PARAM` on a statement that is not a
    /// canonical procedure call.
    InvalidUseOfDefaultParam,
    /// `StrLen_or_Ind` is a negative value that is not a valid input length.
    InvalidLength(SqlLen),
    /// `ColumnSize` cannot be expressed as a T-SQL declaration for `SqlType`.
    InvalidParameterSize(usize),
    /// `DecimalDigits` cannot be expressed as a T-SQL scale for `SqlType`.
    InvalidDecimalDigits(SqlSmallInt),
    /// The SQL type cannot be materialised as a typed NULL.
    UnsupportedSqlType(SqlSmallInt),
}

impl ParamBuildError {
    pub(crate) fn diag(self) -> DiagMsg {
        match self {
            Self::UnsupportedCType(_) => ERR_PARAM_C_TYPE_NOT_IMPLEMENTED,
            Self::DataAtExecUnsupported => ERR_DATA_AT_EXEC_NOT_IMPLEMENTED,
            Self::InvalidUseOfDefaultParam => ERR_INVALID_USE_OF_DEFAULT_PARAM,
            Self::InvalidLength(_) => ERR_INVALID_STRING_OR_BUFFER_LENGTH,
            Self::InvalidParameterSize(_) => ERR_INVALID_PARAM_COLUMN_SIZE,
            Self::InvalidDecimalDigits(_) => ERR_INVALID_PARAM_DECIMAL_DIGITS,
            Self::UnsupportedSqlType(_) => ERR_PARAM_SQL_TYPE_NOT_IMPLEMENTED,
        }
    }
}

/// Converts a bound parameter into a named (`@P1`-style) RPC parameter.
///
/// # Safety
/// See [`bound_param_to_value`].
pub(crate) unsafe fn bound_param_to_rpc(
    name: String,
    param: &BoundParam,
) -> Result<RpcParameter, ParamBuildError> {
    let (value, type_metadata) = unsafe { bound_param_to_value(param) }?;
    let parameter = RpcParameter::new(Some(name), StatusFlags::NONE, value);
    Ok(match type_metadata {
        Some(metadata) => parameter.with_type_metadata(metadata),
        None => parameter,
    })
}

/// Reads the application's value buffer and produces the corresponding
/// [`SqlType`].
///
/// # Safety
/// `param.parameter_value_ptr` and `param.strlen_or_ind_ptr` must satisfy the
/// ODBC binding contract: the value buffer is readable for the indicated
/// length and the indicator pointer, if non-null, points to one valid `SqlLen`.
pub(crate) unsafe fn bound_param_to_value(
    param: &BoundParam,
) -> Result<TypedValue, ParamBuildError> {
    let indicator = if param.strlen_or_ind_ptr.is_null() {
        None
    } else {
        Some(unsafe { *param.strlen_or_ind_ptr })
    };

    if let Some(ind) = indicator {
        if ind == SQL_NULL_DATA {
            return null_value(param);
        }
        if ind == SQL_DEFAULT_PARAM {
            return Err(ParamBuildError::InvalidUseOfDefaultParam);
        }
        if ind == SQL_DATA_AT_EXEC || ind <= SQL_LEN_DATA_AT_EXEC_OFFSET {
            return Err(ParamBuildError::DataAtExecUnsupported);
        }
        // Any remaining negative indicator  is invalid for an input parameter
        if ind < 0 && ind != SQL_NTS as SqlLen {
            return Err(ParamBuildError::InvalidLength(ind));
        }
    }

    // For string C types a null indicator pointer means "null-terminated".
    let len_spec = indicator.unwrap_or(SQL_NTS as SqlLen);

    // A defaulted binding describes its value through `sql_type`, but the match
    // below reads only `c_type`. `resolve_default_c_type` maps `decimal`,
    // `numeric`, `sql_variant` and `xml` onto a character C type, so without
    // this guard those would be read as text and sent as `varchar(max)` /
    // `nvarchar(max)` -- a type the application never asked for, and one the
    // server cannot assign to a `sql_variant` at all. Only the character SQL
    // types genuinely describe a character buffer.
    if param.c_type_defaulted
        && !matches!(
            param.sql_type,
            SQL_CHAR | SQL_VARCHAR | SQL_LONGVARCHAR | SQL_WCHAR | SQL_WVARCHAR | SQL_WLONGVARCHAR
        )
    {
        return Err(ParamBuildError::UnsupportedCType(param.c_type));
    }

    let value = match param.c_type {
        SQL_C_CHAR => {
            let bytes =
                unsafe { read_char_bytes(param.parameter_value_ptr as *const u8, len_spec) };
            let text = String::from_utf8_lossy(&bytes).into_owned();
            SqlType::VarcharMax(Some(SqlString::from_utf8_string(text)))
        }
        SQL_C_WCHAR => {
            let bytes =
                unsafe { read_wchar_bytes(param.parameter_value_ptr as *const u16, len_spec) };
            SqlType::NVarcharMax(Some(SqlString::new(bytes, EncodingType::Utf16)))
        }
        SQL_C_BINARY => {
            let bytes = unsafe { read_binary_bytes(param, len_spec)? };
            SqlType::VarBinaryMax(Some(bytes))
        }
        // Non-character default C types reach here only through an explicit
        // binding; this driver does not yet convert their buffers.
        other => return Err(ParamBuildError::UnsupportedCType(other)),
    };

    Ok((value, None))
}

/// Returns `true` when `indicator` is a data-at-execution value
/// (`SQL_DATA_AT_EXEC` or any value at or below `SQL_LEN_DATA_AT_EXEC_OFFSET`).
pub(crate) fn is_data_at_exec_indicator(indicator: SqlLen) -> bool {
    indicator == SQL_DATA_AT_EXEC || indicator <= SQL_LEN_DATA_AT_EXEC_OFFSET
}

/// Builds the streaming placeholder type for a data-at-execution bound
/// parameter. The actual bytes arrive later via `SQLPutData`.
pub(crate) fn dae_placeholder_type(
    c_type: SqlSmallInt,
) -> Result<StreamedSqlType, ParamBuildError> {
    match c_type {
        SQL_C_CHAR => Ok(StreamedSqlType::VarcharMax),
        SQL_C_WCHAR => Ok(StreamedSqlType::NVarcharMax),
        SQL_C_BINARY => Ok(StreamedSqlType::VarBinaryMax),
        other => Err(ParamBuildError::UnsupportedCType(other)),
    }
}
/// A TDS value plus the precision/scale the RPC layer must use for both the
/// `@P1 <type>` declaration and the wire `TYPE_INFO`.
type TypedValue = (SqlType, Option<RpcTypeMetadata>);

/// Longest non-`max` length of the narrow character and binary types.
const MAX_NARROW_LENGTH: usize = 8000;
/// Longest non-`max` length of the wide character types.
const MAX_WIDE_LENGTH: usize = 4000;
/// T-SQL `decimal`/`numeric` precision bounds.
const PRECISION_RANGE: std::ops::RangeInclusive<usize> = 1..=38;
/// Largest fractional-seconds scale of `time`/`datetime2`/`datetimeoffset`.
const MAX_DATETIME_SCALE: u8 = 7;

/// Typed NULL for a bound parameter.
///
/// A binding made with `SQL_C_DEFAULT` carries its type in `sql_type`, so the
/// NULL is built from that: `SQL_DECIMAL` resolves to a `SQL_C_CHAR` buffer, and
/// declaring that parameter `varchar` would send the server the wrong type.
/// An explicit character binding keeps its C type, which the conversion matrix
/// has already paired with a character SQL type.
fn null_value(param: &BoundParam) -> Result<TypedValue, ParamBuildError> {
    if param.c_type_defaulted {
        return default_typed_null(param.sql_type, param.column_size, param.decimal_digits);
    }
    match param.c_type {
        SQL_C_CHAR => Ok((SqlType::VarcharMax(None), None)),
        SQL_C_WCHAR => Ok((SqlType::NVarcharMax(None), None)),
        SQL_C_BINARY => Ok((SqlType::VarBinaryMax(None), None)),
        other => Err(ParamBuildError::UnsupportedCType(other)),
    }
}

/// Builds the typed NULL a `SQL_C_DEFAULT` binding describes.
///
/// `column_size` and `decimal_digits` come straight from the application, so
/// every value that participates in the `@P1 <type>` declaration is validated
/// here: emitting `decimal(0,0)` or `char(0)` would otherwise fail server-side
/// with an opaque syntax error instead of `HY104` at execute time.
///
/// The returned [`RpcTypeMetadata`] is the *only* place precision and scale are
/// carried. [`RpcParameter`] uses it to render the declaration and to write the
/// wire `TYPE_INFO`, so the two cannot drift apart.
fn default_typed_null(
    sql_type: SqlSmallInt,
    column_size: usize,
    decimal_digits: SqlSmallInt,
) -> Result<TypedValue, ParamBuildError> {
    let value = match sql_type {
        SQL_BIT => SqlType::Bit(None),
        SQL_TINYINT => SqlType::TinyInt(None),
        SQL_SMALLINT => SqlType::SmallInt(None),
        SQL_INTEGER => SqlType::Int(None),
        SQL_BIGINT => SqlType::BigInt(None),
        SQL_REAL => SqlType::Real(None),
        SQL_FLOAT | SQL_DOUBLE => SqlType::Float(None),
        SQL_DECIMAL => {
            let metadata = decimal_metadata(column_size, decimal_digits)?;
            return Ok((SqlType::Decimal(None), Some(metadata)));
        }
        SQL_NUMERIC => {
            let metadata = decimal_metadata(column_size, decimal_digits)?;
            return Ok((SqlType::Numeric(None), Some(metadata)));
        }
        SQL_CHAR => SqlType::Char(None, fixed_length(column_size, MAX_NARROW_LENGTH)?),
        SQL_VARCHAR => match variable_length(column_size, MAX_NARROW_LENGTH) {
            Some(length) => SqlType::Varchar(None, length),
            None => SqlType::VarcharMax(None),
        },
        SQL_LONGVARCHAR => SqlType::Text(None),
        SQL_WCHAR => SqlType::NChar(None, fixed_length(column_size, MAX_WIDE_LENGTH)?),
        SQL_WVARCHAR => match variable_length(column_size, MAX_WIDE_LENGTH) {
            Some(length) => SqlType::NVarchar(None, length),
            None => SqlType::NVarcharMax(None),
        },
        SQL_WLONGVARCHAR => SqlType::NText(None),
        SQL_BINARY => SqlType::Binary(None, fixed_length(column_size, MAX_NARROW_LENGTH)?),
        SQL_VARBINARY => match variable_length(column_size, MAX_NARROW_LENGTH) {
            Some(length) => SqlType::VarBinary(None, length),
            None => SqlType::VarBinaryMax(None),
        },
        SQL_LONGVARBINARY => SqlType::VarBinaryMax(None),
        SQL_GUID => SqlType::Uuid(None),
        SQL_TYPE_DATE => SqlType::Date(None),
        SQL_TYPE_TIME | SQL_SS_TIME2 => {
            let metadata = datetime_metadata(decimal_digits)?;
            return Ok((SqlType::Time(None), Some(metadata)));
        }
        SQL_TYPE_TIMESTAMP => {
            let metadata = datetime_metadata(decimal_digits)?;
            return Ok((SqlType::DateTime2(None), Some(metadata)));
        }
        SQL_SS_TIMESTAMPOFFSET => {
            let metadata = datetime_metadata(decimal_digits)?;
            return Ok((SqlType::DateTimeOffset(None), Some(metadata)));
        }
        SQL_SS_XML => SqlType::Xml(None),
        // A NULL `sql_variant` carries no payload, so the inner type only has to
        // be a legal one - it never reaches the wire.
        SQL_SS_VARIANT => SqlType::Variant(Box::new(SqlType::Varchar(None, 1))),
        SQL_SS_VECTOR => {
            let (dimensions, base_type) = vector_metadata(column_size, decimal_digits)?;
            SqlType::Vector(None, dimensions, base_type)
        }
        // `SQL_SS_UDT` and `SQL_SS_TABLE` need the fully qualified server type
        // name, which `SQLDescribeParam` does not report and this driver has no
        // other way to obtain, so they are rejected up front at bind time.
        other => return Err(ParamBuildError::UnsupportedSqlType(other)),
    };
    Ok((value, None))
}

/// Length of a fixed-width `char`/`nchar`/`binary` declaration. Zero-length and
/// oversized declarations are invalid T-SQL and have no `max` spelling.
///
/// Matches msodbcsql for ODBC 3.x applications: `CheckSqlPrec`
/// (`Sql/Ntdbms/sqlncli/odbc/sqlcdesc.cpp`) rejects a zero `ColumnSize` on these
/// types with `HY104`, and only clamps it to the maximum for a 2.x application
/// (`IS2xAPP`). We report the same `HY104`, at execute rather than at bind.
/// `varchar`/`nvarchar` differ deliberately -- see [`variable_length`].
fn fixed_length(column_size: usize, max: usize) -> Result<u16, ParamBuildError> {
    u16::try_from(column_size)
        .ok()
        .filter(|_| (1..=max).contains(&column_size))
        .ok_or(ParamBuildError::InvalidParameterSize(column_size))
}

/// Length of a `varchar`/`nvarchar`/`varbinary` declaration, or `None` for the
/// `max` spelling.
///
/// `SQLDescribeParam` reports 0 for `*(max)` parameters, and an application may
/// legitimately pass a `ColumnSize` past the non-`max` limit; both widen to
/// `max` rather than erroring, matching `RpcParameter::get_sql_name`.
///
/// Also matches msodbcsql, which skips precision validation entirely for
/// `SQL_VARCHAR`/`SQL_WVARCHAR` and uses the data length instead
/// (`Sql/Ntdbms/sqlncli/odbc/sqlcmisc.cpp`, the `wSqlType != SQL_WVARCHAR &&
/// wSqlType != SQL_VARCHAR` guard before `FixupColumnSizeDecimalDigits`).
fn variable_length(column_size: usize, max: usize) -> Option<u16> {
    if column_size == 0 || column_size > max {
        None
    } else {
        u16::try_from(column_size).ok()
    }
}

fn decimal_metadata(
    column_size: usize,
    decimal_digits: SqlSmallInt,
) -> Result<RpcTypeMetadata, ParamBuildError> {
    let precision = u8::try_from(column_size)
        .ok()
        .filter(|_| PRECISION_RANGE.contains(&column_size))
        .ok_or(ParamBuildError::InvalidParameterSize(column_size))?;
    let scale = u8::try_from(decimal_digits)
        .ok()
        .filter(|scale| *scale <= precision)
        .ok_or(ParamBuildError::InvalidDecimalDigits(decimal_digits))?;
    Ok(RpcTypeMetadata {
        precision: Some(precision),
        scale: Some(scale),
    })
}

fn datetime_metadata(decimal_digits: SqlSmallInt) -> Result<RpcTypeMetadata, ParamBuildError> {
    let scale = u8::try_from(decimal_digits)
        .ok()
        .filter(|scale| *scale <= MAX_DATETIME_SCALE)
        .ok_or(ParamBuildError::InvalidDecimalDigits(decimal_digits))?;
    Ok(RpcTypeMetadata {
        precision: None,
        scale: Some(scale),
    })
}

/// Recovers a vector's dimension count and base type from the `ColumnSize` and
/// `DecimalDigits` that `SQLDescribeParam` reported.
///
/// `ColumnSize` is the size of the whole client buffer - a
/// [`SqlSsVectorLayout`] header followed by `dimensions` elements. msodbcsql
/// always exchanges those elements as 4-byte floats regardless of the
/// server-side base type, so the element width is
/// [`SQL_SS_VECTOR_ELEMENT_SIZE`] and not the base type's own width.
/// `DecimalDigits` carries the base type (`0` = float32, `1` = float16),
/// mirroring `SQL_SS_VECTOR`'s `SQL_CA_SS_VECTOR_BASE_TYPE` descriptor field.
fn vector_metadata(
    column_size: usize,
    base_type: SqlSmallInt,
) -> Result<(u16, VectorBaseType), ParamBuildError> {
    let payload_size = column_size
        .checked_sub(std::mem::size_of::<SqlSsVectorLayout>())
        .filter(|size| size % SQL_SS_VECTOR_ELEMENT_SIZE == 0)
        .ok_or(ParamBuildError::InvalidParameterSize(column_size))?;
    let dimensions = u16::try_from(payload_size / SQL_SS_VECTOR_ELEMENT_SIZE)
        .map_err(|_| ParamBuildError::InvalidParameterSize(column_size))?;
    let base_type = match base_type {
        0 => VectorBaseType::Float32,
        1 => VectorBaseType::Float16,
        _ => return Err(ParamBuildError::InvalidDecimalDigits(base_type)),
    };
    Ok((dimensions, base_type))
}

/// Reads narrow (`SQL_C_CHAR`) bytes. `len_spec` is a byte count, or `SQL_NTS`
/// for a NUL-terminated string.
///
/// # Safety
/// `ptr`, if non-null, must be readable for the resolved length (or up to the
/// first NUL when `len_spec == SQL_NTS`).
unsafe fn read_char_bytes(ptr: *const u8, len_spec: SqlLen) -> Vec<u8> {
    if ptr.is_null() {
        return Vec::new();
    }
    let len = if len_spec == SQL_NTS as SqlLen {
        let mut n = 0usize;
        while unsafe { *ptr.add(n) } != 0 {
            n += 1;
        }
        n
    } else if len_spec < 0 {
        0
    } else {
        len_spec as usize
    };
    unsafe { slice::from_raw_parts(ptr, len).to_vec() }
}

/// Reads raw (`SQL_C_BINARY`) bytes.
///
/// Binary buffers have no terminator, so the length must be stated. `len_spec`
/// is the indicator value when the application supplied an indicator pointer;
/// `SQL_NTS` here means it did not, and the ODBC contract falls back to
/// `BufferLength`. A binding that states neither has no readable extent, so it
/// is rejected rather than guessed at.
///
/// # Safety
/// `param.parameter_value_ptr`, if non-null, must be readable for the resolved
/// byte count.
unsafe fn read_binary_bytes(
    param: &BoundParam,
    len_spec: SqlLen,
) -> Result<Vec<u8>, ParamBuildError> {
    let len = if len_spec == SQL_NTS as SqlLen {
        if param.buffer_length < 0 {
            return Err(ParamBuildError::InvalidLength(param.buffer_length));
        }
        param.buffer_length
    } else {
        len_spec
    };
    if param.parameter_value_ptr.is_null() || len == 0 {
        return Ok(Vec::new());
    }
    Ok(
        unsafe { slice::from_raw_parts(param.parameter_value_ptr as *const u8, len as usize) }
            .to_vec(),
    )
}

/// Reads wide (`SQL_C_WCHAR`) data as UTF-16LE bytes. `len_spec` is a **byte**
/// count per the ODBC spec, or `SQL_NTS` for a NUL-terminated string.
///
/// # Safety
/// `ptr`, if non-null, must be readable for the resolved number of `u16` units
/// (or up to the first NUL when `len_spec == SQL_NTS`).
unsafe fn read_wchar_bytes(ptr: *const u16, len_spec: SqlLen) -> Vec<u8> {
    if ptr.is_null() {
        return Vec::new();
    }
    let units = if len_spec == SQL_NTS as SqlLen {
        let mut n = 0usize;
        while unsafe { *ptr.add(n) } != 0 {
            n += 1;
        }
        n
    } else if len_spec < 0 {
        0
    } else {
        (len_spec as usize) / std::mem::size_of::<u16>()
    };
    let slice = unsafe { slice::from_raw_parts(ptr, units) };
    slice.iter().flat_map(|u| u.to_le_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{SQL_C_LONG, SQL_NO_TOTAL, SQL_PARAM_INPUT, SQL_SS_UDT, SqlULen};
    use std::ffi::c_void;

    /// A `ColumnSize` past every non-`max` limit, as an application binding an
    /// unbounded value may plausibly pass. Not an ODBC constant: msodbcsql's
    /// unbounded sentinel is `0`, so there is no header name for this value.
    const OVERSIZED_COLUMN_SIZE: SqlULen = 2_147_483_647;

    fn param(c_type: SqlSmallInt, ptr: *mut c_void, ind: *mut SqlLen) -> BoundParam {
        BoundParam {
            input_output_type: SQL_PARAM_INPUT,
            c_type,
            c_type_defaulted: false,
            sql_type: 0,
            column_size: 0,
            decimal_digits: 0,
            parameter_value_ptr: ptr,
            buffer_length: 0,
            strlen_or_ind_ptr: ind,
        }
    }

    /// A parameter bound with `SQL_C_DEFAULT`. `SQLBindParameter` has already
    /// resolved `c_type` to the SQL type's default C type, so only the flag
    /// distinguishes it from an explicit bind of that same C type.
    fn default_param(sql_type: SqlSmallInt, ind: *mut SqlLen) -> BoundParam {
        let mut p = param(SQL_C_CHAR, std::ptr::null_mut(), ind);
        p.c_type_defaulted = true;
        p.sql_type = sql_type;
        p
    }

    #[test]
    fn char_nts_becomes_varchar() {
        let mut buf: Vec<u8> = b"hello\0".to_vec();
        let mut ind: SqlLen = SQL_NTS as SqlLen;
        let p = param(SQL_C_CHAR, buf.as_mut_ptr() as *mut c_void, &mut ind);
        let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
        match value {
            SqlType::VarcharMax(Some(s)) => assert_eq!(s.to_utf8_string(), "hello"),
            other => panic!("expected VarcharMax(Some), got {other:?}"),
        }
    }

    #[test]
    fn binary_with_indicator_length_becomes_varbinary() {
        let mut buf: Vec<u8> = vec![0x01, 0x00, 0xFF, 0x7E];
        let mut ind: SqlLen = 4;
        let p = param(SQL_C_BINARY, buf.as_mut_ptr() as *mut c_void, &mut ind);
        let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
        match value {
            SqlType::VarBinaryMax(Some(b)) => assert_eq!(b, vec![0x01, 0x00, 0xFF, 0x7E]),
            other => panic!("expected VarBinaryMax(Some), got {other:?}"),
        }
    }

    /// Without an indicator pointer a binary buffer has no stated length, so
    /// the binding's `BufferLength` supplies it.
    #[test]
    fn binary_without_indicator_uses_buffer_length() {
        let mut buf: Vec<u8> = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let mut p = param(
            SQL_C_BINARY,
            buf.as_mut_ptr() as *mut c_void,
            std::ptr::null_mut(),
        );
        p.buffer_length = 2;
        let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
        match value {
            SqlType::VarBinaryMax(Some(b)) => assert_eq!(b, vec![0xDE, 0xAD]),
            other => panic!("expected VarBinaryMax(Some), got {other:?}"),
        }
    }

    #[test]
    fn binary_null_indicator_becomes_typed_null() {
        let mut ind: SqlLen = SQL_NULL_DATA;
        let p = param(SQL_C_BINARY, std::ptr::null_mut(), &mut ind);
        let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
        assert!(matches!(value, SqlType::VarBinaryMax(None)));
    }

    #[test]
    fn wchar_explicit_length_becomes_nvarchar() {
        let mut buf: Vec<u16> = "hi".encode_utf16().collect();
        let mut ind: SqlLen = (buf.len() * 2) as SqlLen;
        let p = param(SQL_C_WCHAR, buf.as_mut_ptr() as *mut c_void, &mut ind);
        let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
        match value {
            SqlType::NVarcharMax(Some(s)) => assert_eq!(s.to_utf8_string(), "hi"),
            other => panic!("expected NVarcharMax(Some), got {other:?}"),
        }
    }

    #[test]
    fn null_indicator_yields_typed_null() {
        let mut ind: SqlLen = SQL_NULL_DATA;
        let p = param(SQL_C_CHAR, std::ptr::null_mut(), &mut ind);
        let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
        assert!(matches!(value, SqlType::VarcharMax(None)));
    }

    #[test]
    fn unsupported_c_type_is_rejected() {
        let mut ind: SqlLen = 4;
        let mut val: i32 = 7;
        let p = param(SQL_C_LONG, &mut val as *mut i32 as *mut c_void, &mut ind);
        let err = unsafe { bound_param_to_value(&p) }.unwrap_err();
        assert_eq!(err, ParamBuildError::UnsupportedCType(SQL_C_LONG));
    }

    #[test]
    fn data_at_exec_is_rejected() {
        let mut ind: SqlLen = SQL_DATA_AT_EXEC;
        let p = param(SQL_C_CHAR, std::ptr::null_mut(), &mut ind);
        let err = unsafe { bound_param_to_value(&p) }.unwrap_err();
        assert_eq!(err, ParamBuildError::DataAtExecUnsupported);
    }

    #[test]
    fn invalid_indicator_is_rejected() {
        let mut ind: SqlLen = SQL_NO_TOTAL;
        let p = param(SQL_C_CHAR, std::ptr::null_mut(), &mut ind);
        let err = unsafe { bound_param_to_value(&p) }.unwrap_err();
        assert_eq!(err, ParamBuildError::InvalidLength(SQL_NO_TOTAL));
    }

    /// `SQL_DEFAULT_PARAM` is only legal for a canonical procedure call, which
    /// this driver does not support, so it is 07S01 rather than "not yet
    /// implemented" (msodbcsql `sqlccmd.cpp` -> IDS_07_S01).
    #[test]
    fn default_param_indicator_is_invalid_use_not_unimplemented() {
        let mut ind: SqlLen = SQL_DEFAULT_PARAM;
        let p = param(SQL_C_CHAR, std::ptr::null_mut(), &mut ind);
        let err = unsafe { bound_param_to_value(&p) }.unwrap_err();
        assert_eq!(err, ParamBuildError::InvalidUseOfDefaultParam);
        assert_eq!(err.diag().state, *b"07S01");
    }

    #[test]
    fn null_indicator_wchar_yields_typed_null() {
        let mut ind: SqlLen = SQL_NULL_DATA;
        let p = param(SQL_C_WCHAR, std::ptr::null_mut(), &mut ind);
        let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
        assert!(matches!(value, SqlType::NVarcharMax(None)));
    }

    #[test]
    fn null_indicator_unsupported_c_type_is_rejected() {
        let mut ind: SqlLen = SQL_NULL_DATA;
        let p = param(SQL_C_LONG, std::ptr::null_mut(), &mut ind);
        let err = unsafe { bound_param_to_value(&p) }.unwrap_err();
        assert_eq!(err, ParamBuildError::UnsupportedCType(SQL_C_LONG));
    }

    #[test]
    fn default_null_uses_described_sql_type() {
        let mut ind: SqlLen = SQL_NULL_DATA;
        let mut p = default_param(SQL_INTEGER, &mut ind);
        p.column_size = 10;
        let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
        assert!(matches!(value, SqlType::Int(None)));

        p.sql_type = SQL_WVARCHAR;
        p.column_size = 40;
        let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
        assert!(matches!(value, SqlType::NVarchar(None, 40)));
    }

    /// Non-NULL defaulted binds are only readable for the character SQL types.
    ///
    /// `resolve_default_c_type` maps `decimal`, `numeric`, `sql_variant` and
    /// `xml` onto `SQL_C_CHAR`/`SQL_C_WCHAR`, so without an explicit guard the
    /// `c_type` match would read their buffers as text and send `varchar(max)`
    /// or `nvarchar(max)`. `sql_variant` is the sharp edge: the server cannot
    /// assign `varchar(max)` to it, so the application would see an opaque
    /// server error instead of `HYC00`.
    #[test]
    fn default_non_null_rejects_sql_types_that_borrow_a_character_c_type() {
        let mut buf: Vec<u8> = b"1.5\0".to_vec();
        for (sql_type, c_type) in [
            (SQL_DECIMAL, SQL_C_CHAR),
            (SQL_NUMERIC, SQL_C_CHAR),
            (SQL_SS_VARIANT, SQL_C_CHAR),
            (SQL_SS_XML, SQL_C_WCHAR),
        ] {
            let mut ind: SqlLen = SQL_NTS as SqlLen;
            let mut p = default_param(sql_type, &mut ind);
            p.c_type = c_type;
            p.parameter_value_ptr = buf.as_mut_ptr() as *mut c_void;
            assert!(
                matches!(
                    unsafe { bound_param_to_value(&p) },
                    Err(ParamBuildError::UnsupportedCType(_))
                ),
                "sql_type {sql_type} should not be read as a character buffer"
            );
        }
    }

    /// The guard above must not reject the character SQL types, which are the
    /// ones a defaulted bind really can describe with a character buffer.
    #[test]
    fn default_non_null_reads_character_sql_types() {
        let mut buf: Vec<u8> = b"hello\0".to_vec();
        let mut ind: SqlLen = SQL_NTS as SqlLen;
        let mut p = default_param(SQL_VARCHAR, &mut ind);
        p.parameter_value_ptr = buf.as_mut_ptr() as *mut c_void;
        let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
        match value {
            SqlType::VarcharMax(Some(s)) => assert_eq!(s.to_utf8_string(), "hello"),
            other => panic!("expected VarcharMax(Some), got {other:?}"),
        }
    }

    /// The typed NULL and the precision/scale metadata must be produced
    /// together, so the declaration `RpcParameter` renders cannot disagree with
    /// the value it serializes. (That the metadata then drives both is covered
    /// by `type_metadata_drives_declaration_and_wire_metadata` in `mssql-tds`.)
    #[test]
    fn default_null_pairs_value_with_metadata() {
        let decimal = |precision, scale| {
            Some(RpcTypeMetadata {
                precision: Some(precision),
                scale: Some(scale),
            })
        };
        let temporal = |scale| {
            Some(RpcTypeMetadata {
                precision: None,
                scale: Some(scale),
            })
        };
        let cases: &[(
            SqlSmallInt,
            usize,
            SqlSmallInt,
            SqlType,
            Option<RpcTypeMetadata>,
        )] = &[
            (SQL_DECIMAL, 12, 3, SqlType::Decimal(None), decimal(12, 3)),
            (SQL_NUMERIC, 38, 0, SqlType::Numeric(None), decimal(38, 0)),
            (SQL_SS_TIME2, 16, 4, SqlType::Time(None), temporal(4)),
            (
                SQL_TYPE_TIMESTAMP,
                27,
                7,
                SqlType::DateTime2(None),
                temporal(7),
            ),
            (
                SQL_SS_TIMESTAMPOFFSET,
                34,
                7,
                SqlType::DateTimeOffset(None),
                temporal(7),
            ),
            (SQL_INTEGER, 10, 0, SqlType::Int(None), None),
            (SQL_CHAR, 10, 0, SqlType::Char(None, 10), None),
            (SQL_WVARCHAR, 40, 0, SqlType::NVarchar(None, 40), None),
            // An oversized `ColumnSize` and `i32::MAX` both mean `max`.
            (
                SQL_WVARCHAR,
                OVERSIZED_COLUMN_SIZE,
                0,
                SqlType::NVarcharMax(None),
                None,
            ),
            (SQL_VARCHAR, 9000, 0, SqlType::VarcharMax(None), None),
            (
                SQL_VARBINARY,
                OVERSIZED_COLUMN_SIZE,
                0,
                SqlType::VarBinaryMax(None),
                None,
            ),
        ];
        for (sql_type, column_size, decimal_digits, expected_value, expected_metadata) in cases {
            let mut ind: SqlLen = SQL_NULL_DATA;
            let mut p = default_param(*sql_type, &mut ind);
            p.column_size = *column_size;
            p.decimal_digits = *decimal_digits;
            let (value, metadata) = unsafe { bound_param_to_value(&p) }
                .unwrap_or_else(|e| panic!("conversion failed for {sql_type}: {e:?}"));
            assert_eq!(&value, expected_value, "case: sql_type {sql_type}");
            assert_eq!(&metadata, expected_metadata, "case: sql_type {sql_type}");
        }
    }

    /// A `ColumnSize`/`DecimalDigits` that has no legal T-SQL spelling is
    /// rejected here rather than sent as a malformed declaration.
    #[test]
    fn default_null_rejects_undeclarable_metadata() {
        let cases: &[(SqlSmallInt, usize, SqlSmallInt, ParamBuildError)] = &[
            (SQL_DECIMAL, 0, 0, ParamBuildError::InvalidParameterSize(0)),
            (
                SQL_DECIMAL,
                39,
                0,
                ParamBuildError::InvalidParameterSize(39),
            ),
            // Scale may not exceed precision.
            (SQL_NUMERIC, 5, 6, ParamBuildError::InvalidDecimalDigits(6)),
            (SQL_CHAR, 0, 0, ParamBuildError::InvalidParameterSize(0)),
            (
                SQL_WCHAR,
                4001,
                0,
                ParamBuildError::InvalidParameterSize(4001),
            ),
            (SQL_BINARY, 0, 0, ParamBuildError::InvalidParameterSize(0)),
            (
                SQL_TYPE_TIMESTAMP,
                27,
                8,
                ParamBuildError::InvalidDecimalDigits(8),
            ),
            (
                SQL_SS_TIME2,
                16,
                -1,
                ParamBuildError::InvalidDecimalDigits(-1),
            ),
            (
                SQL_SS_UDT,
                0,
                0,
                ParamBuildError::UnsupportedSqlType(SQL_SS_UDT),
            ),
        ];
        for &(sql_type, column_size, decimal_digits, expected) in cases {
            let mut ind: SqlLen = SQL_NULL_DATA;
            let mut p = default_param(sql_type, &mut ind);
            p.column_size = column_size;
            p.decimal_digits = decimal_digits;
            let err = unsafe { bound_param_to_value(&p) }
                .expect_err(&format!("expected rejection for sql_type {sql_type}"));
            assert_eq!(err, expected, "case: sql_type {sql_type}");
        }
    }

    /// A vector's `ColumnSize` is the client buffer size: header + 4 bytes per
    /// dimension, regardless of the server-side base type.
    #[test]
    fn vector_metadata_round_trips_dimensions() {
        let header = std::mem::size_of::<SqlSsVectorLayout>();
        assert_eq!(
            vector_metadata(header + 3 * SQL_SS_VECTOR_ELEMENT_SIZE, 0).unwrap(),
            (3, VectorBaseType::Float32)
        );
        assert_eq!(
            vector_metadata(header + 3 * SQL_SS_VECTOR_ELEMENT_SIZE, 1).unwrap(),
            (3, VectorBaseType::Float16)
        );
        // Too small for the header, and a payload that is not a whole number of
        // elements, are both rejected.
        assert_eq!(
            vector_metadata(1, 0).unwrap_err(),
            ParamBuildError::InvalidParameterSize(1)
        );
        assert_eq!(
            vector_metadata(header + 3, 0).unwrap_err(),
            ParamBuildError::InvalidParameterSize(header + 3)
        );
        assert_eq!(
            vector_metadata(header, 2).unwrap_err(),
            ParamBuildError::InvalidDecimalDigits(2)
        );
    }

    #[test]
    fn read_char_bytes_edge_cases() {
        assert!(unsafe { read_char_bytes(std::ptr::null(), 5) }.is_empty());
        let buf = b"abc";
        // Negative (non-NTS) length yields no bytes.
        assert!(unsafe { read_char_bytes(buf.as_ptr(), -5) }.is_empty());
        // Explicit positive length reads exactly that many bytes.
        assert_eq!(unsafe { read_char_bytes(buf.as_ptr(), 3) }, b"abc");
    }

    #[test]
    fn read_wchar_bytes_edge_cases() {
        assert!(unsafe { read_wchar_bytes(std::ptr::null(), 5) }.is_empty());
        let units: Vec<u16> = "hi".encode_utf16().chain(std::iter::once(0)).collect();
        // SQL_NTS reads u16 units up to the NUL terminator.
        assert_eq!(
            unsafe { read_wchar_bytes(units.as_ptr(), SQL_NTS as SqlLen) },
            vec![b'h', 0, b'i', 0]
        );
        // Negative (non-NTS) length yields no bytes.
        assert!(unsafe { read_wchar_bytes(units.as_ptr(), -5) }.is_empty());
    }
}
