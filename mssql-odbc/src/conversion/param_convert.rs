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
//! A `SQL_NULL_DATA` parameter is materialised as a typed TDS NULL from
//! `sql_type` -- see [`typed_null`].

use mssql_tds::datatypes::sql_string::{EncodingType, SqlString};
use mssql_tds::datatypes::sqldatatypes::VectorBaseType;
use mssql_tds::datatypes::sqltypes::SqlType;
use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, RpcTypeMetadata, StatusFlags};

use crate::api::odbc_types::{
    SQL_BIGINT, SQL_BINARY, SQL_BIT, SQL_CHAR, SQL_DECIMAL, SQL_DOUBLE, SQL_FLOAT, SQL_GUID,
    SQL_INTEGER, SQL_LONGVARBINARY, SQL_LONGVARCHAR, SQL_NUMERIC, SQL_REAL, SQL_SMALLINT,
    SQL_SS_TIME2, SQL_SS_TIMESTAMPOFFSET, SQL_SS_VARIANT, SQL_SS_VECTOR,
    SQL_SS_VECTOR_ELEMENT_SIZE, SQL_SS_XML, SQL_TINYINT, SQL_TYPE_DATE, SQL_TYPE_TIME,
    SQL_TYPE_TIMESTAMP, SQL_VARBINARY, SQL_VARCHAR, SQL_WCHAR, SQL_WLONGVARCHAR, SQL_WVARCHAR,
    SqlLen, SqlSmallInt, SqlSsVectorLayout,
};
use crate::api::sqlstate::{
    DiagMsg, ERR_DATA_AT_EXEC_NOT_IMPLEMENTED, ERR_INVALID_CHARACTER_VALUE,
    ERR_INVALID_NULL_POINTER, ERR_INVALID_PARAM_PRECISION_OR_SCALE,
    ERR_INVALID_STRING_OR_BUFFER_LENGTH, ERR_INVALID_USE_OF_DEFAULT_PARAM,
    ERR_NUMERIC_OUT_OF_RANGE, ERR_PARAM_C_TYPE_NOT_IMPLEMENTED, ERR_PARAM_SQL_TYPE_NOT_IMPLEMENTED,
    ERR_RESTRICTED_DATA_TYPE,
};
use crate::conversion::error::ConvError;
use crate::conversion::numeric::narrow_i128;
use crate::conversion::param_buffer::{AppValue, Indicator, read_indicator, read_param_value};
use crate::params::BoundParam;

/// Why a bound parameter could not be turned into an RPC parameter.
///
/// Each variant carries its own SQLSTATE through [`diag`]. All but [`Value`]
/// describe the binding rather than the data; [`Value`] wraps a
/// [`crate::conversion::error::ConvError`] from the shared value model.
///
/// [`diag`]: ParamBuildError::diag
/// [`Value`]: ParamBuildError::Value
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
    /// The parameter carries a value but `ParameterValuePtr` is null.
    NullValuePointer,
    /// `ColumnSize` cannot be expressed as a T-SQL declaration for `SqlType`.
    InvalidParameterSize(usize),
    /// `DecimalDigits` cannot be expressed as a T-SQL scale for `SqlType`.
    InvalidDecimalDigits(SqlSmallInt),
    /// The SQL type cannot be materialised as a typed NULL.
    UnsupportedSqlType(SqlSmallInt),
    /// The value could not be represented in the target SQL type.
    Value(ConvError),
}

impl ParamBuildError {
    pub(crate) fn diag(self) -> DiagMsg {
        match self {
            Self::UnsupportedCType(_) => ERR_PARAM_C_TYPE_NOT_IMPLEMENTED,
            Self::DataAtExecUnsupported => ERR_DATA_AT_EXEC_NOT_IMPLEMENTED,
            Self::InvalidUseOfDefaultParam => ERR_INVALID_USE_OF_DEFAULT_PARAM,
            Self::InvalidLength(_) => ERR_INVALID_STRING_OR_BUFFER_LENGTH,
            Self::NullValuePointer => ERR_INVALID_NULL_POINTER,
            Self::InvalidParameterSize(_) | Self::InvalidDecimalDigits(_) => {
                ERR_INVALID_PARAM_PRECISION_OR_SCALE
            }
            Self::UnsupportedSqlType(_) => ERR_PARAM_SQL_TYPE_NOT_IMPLEMENTED,
            Self::Value(ConvError::OutOfRange) => ERR_NUMERIC_OUT_OF_RANGE,
            Self::Value(ConvError::InvalidCharacterValue) => ERR_INVALID_CHARACTER_VALUE,
            // Backstop only: parameter legality is settled by the bind-time
            // matrix, and `NotHandledHere` is an internal routing signal that is
            // never meant to reach an application.
            Self::Value(ConvError::Restricted | ConvError::NotHandledHere) => {
                ERR_RESTRICTED_DATA_TYPE
            }
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
    // NULL is settled from the indicator alone, so a typed NULL never reads the
    // value buffer.
    let len_spec = match unsafe { read_indicator(param) }? {
        Indicator::Null => {
            return typed_null(param.sql_type, param.column_size, param.decimal_digits);
        }
        Indicator::Length(len) => len,
    };

    let value = match unsafe { read_param_value(param, len_spec) }? {
        AppValue::Integer(v) => integer_value(param.sql_type, v)?,
        AppValue::NarrowText(bytes) => {
            // `SqlString`'s UTF-8 decode unwraps, so only hand it bytes already
            // checked. Valid input keeps its allocation all the way to the wire.
            let bytes = match String::from_utf8(bytes) {
                Ok(text) => text.into_bytes(),
                Err(e) => String::from_utf8_lossy(e.as_bytes())
                    .into_owned()
                    .into_bytes(),
            };
            SqlType::VarcharMax(Some(SqlString::new(bytes, EncodingType::Utf8)))
        }
        AppValue::WideText(bytes) => {
            SqlType::NVarcharMax(Some(SqlString::new(bytes, EncodingType::Utf16)))
        }
    };

    Ok((value, None))
}

/// Emits the integer `SqlType` named by `ParameterType`, not by the C type, so
/// `@P1` is declared as the application asked. A value outside the target's
/// range is `22003` rather than a silently wrapped wire value.
fn integer_value(sql_type: SqlSmallInt, v: i128) -> Result<SqlType, ParamBuildError> {
    Ok(match sql_type {
        SQL_TINYINT => {
            SqlType::TinyInt(Some(narrow_i128::<u8>(v).map_err(ParamBuildError::Value)?))
        }
        SQL_SMALLINT => {
            SqlType::SmallInt(Some(narrow_i128::<i16>(v).map_err(ParamBuildError::Value)?))
        }
        SQL_INTEGER => SqlType::Int(Some(narrow_i128::<i32>(v).map_err(ParamBuildError::Value)?)),
        SQL_BIGINT => SqlType::BigInt(Some(narrow_i128::<i64>(v).map_err(ParamBuildError::Value)?)),
        other => return Err(ParamBuildError::UnsupportedSqlType(other)),
    })
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
/// The type comes from `sql_type` whether or not the binding was defaulted: the
/// C type only says how the value buffer would have been read, and a NULL has no
/// buffer to read. msodbcsql does the same - `CRPCSQLSender::SendParamList`
/// (`Sql/Ntdbms/sqlncli/odbc/sqlccmd.cpp`) builds the `@Pn <type>` declaration
/// from `lpparam->fSqlType` and never consults the bound C type.
///
/// `column_size` and `decimal_digits` come straight from the application, so
/// every value that participates in the `@P1 <type>` declaration is validated
/// here: emitting `decimal(0,0)` or `char(0)` would otherwise fail server-side
/// with an opaque syntax error instead of `HY104` at execute time.
///
/// The returned [`RpcTypeMetadata`] is the *only* place precision and scale are
/// carried. [`RpcParameter`] uses it to render the declaration and to write the
/// wire `TYPE_INFO`, so the two cannot drift apart.
fn typed_null(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::{
        SQL_C_CHAR, SQL_C_DEFAULT, SQL_C_FLOAT, SQL_C_SLONG, SQL_C_STINYINT, SQL_C_UBIGINT,
        SQL_C_WCHAR, SQL_DATA_AT_EXEC, SQL_DEFAULT_PARAM, SQL_NO_TOTAL, SQL_NTS, SQL_NULL_DATA,
        SQL_PARAM_INPUT, SQL_SS_UDT, SqlULen,
    };
    use crate::params::conversion_matrix::is_supported_conversion;
    use std::ffi::c_void;

    /// A `ColumnSize` past every non-`max` limit, as an application binding an
    /// unbounded value may plausibly pass. Not an ODBC constant: msodbcsql's
    /// unbounded sentinel is `0`, so there is no header name for this value.
    const OVERSIZED_COLUMN_SIZE: SqlULen = 2_147_483_647;

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

    /// A parameter bound with `SQL_C_DEFAULT`, whose `c_type` `SQLBindParameter`
    /// has already resolved - nothing then distinguishes it from an explicit
    /// bind of that same C type.
    fn default_param(sql_type: SqlSmallInt, ind: *mut SqlLen) -> BoundParam {
        let mut p = param(SQL_C_CHAR, std::ptr::null_mut(), ind);
        p.sql_type = sql_type;
        p
    }

    fn int_param(
        c_type: SqlSmallInt,
        sql_type: SqlSmallInt,
        ptr: *mut c_void,
        ind: *mut SqlLen,
    ) -> BoundParam {
        let mut p = param(c_type, ptr, ind);
        p.sql_type = sql_type;
        p
    }

    /// One buffer, four declarations: the wire type follows `ParameterType`, so
    /// the server sees the type the application asked for rather than the width
    /// of the C buffer it happened to bind.
    #[test]
    fn parameter_type_names_the_wire_type_not_the_c_type() {
        let cases: &[(SqlSmallInt, SqlType)] = &[
            (SQL_TINYINT, SqlType::TinyInt(Some(7))),
            (SQL_SMALLINT, SqlType::SmallInt(Some(7))),
            (SQL_INTEGER, SqlType::Int(Some(7))),
            (SQL_BIGINT, SqlType::BigInt(Some(7))),
        ];
        for (sql_type, expected) in cases {
            let mut v: i32 = 7;
            let mut ind: SqlLen = 4;
            let p = int_param(
                SQL_C_SLONG,
                *sql_type,
                &mut v as *mut i32 as *mut c_void,
                &mut ind,
            );
            let (value, metadata) = unsafe { bound_param_to_value(&p) }.unwrap();
            assert_eq!(&value, expected, "case: sql_type {sql_type}");
            assert_eq!(metadata, None, "case: sql_type {sql_type}");
        }
    }

    /// A value the target cannot hold is 22003, not a silently wrapped value.
    #[test]
    fn value_outside_the_target_range_is_22003() {
        let mut v: i32 = 300;
        let mut ind: SqlLen = 4;
        let p = int_param(
            SQL_C_SLONG,
            SQL_TINYINT,
            &mut v as *mut i32 as *mut c_void,
            &mut ind,
        );
        let err = unsafe { bound_param_to_value(&p) }.unwrap_err();
        assert_eq!(err, ParamBuildError::Value(ConvError::OutOfRange));
        assert_eq!(err.diag().state, *b"22003");
    }

    /// `SQL_C_UBIGINT` is the one C type whose range exceeds every SQL target.
    #[test]
    fn ubigint_above_i64_max_is_22003() {
        let mut v: u64 = i64::MAX as u64 + 1;
        let mut ind: SqlLen = 8;
        let p = int_param(
            SQL_C_UBIGINT,
            SQL_BIGINT,
            &mut v as *mut u64 as *mut c_void,
            &mut ind,
        );
        let err = unsafe { bound_param_to_value(&p) }.unwrap_err();
        assert_eq!(err, ParamBuildError::Value(ConvError::OutOfRange));
    }

    #[test]
    fn integer_null_is_typed_by_parameter_type() {
        for c_type in [SQL_C_SLONG, SQL_C_UBIGINT, SQL_C_STINYINT] {
            let mut ind: SqlLen = SQL_NULL_DATA;
            let mut p = int_param(c_type, SQL_INTEGER, std::ptr::null_mut(), &mut ind);
            p.column_size = 10;
            let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
            assert!(matches!(value, SqlType::Int(None)), "c_type {c_type}");
        }
    }

    /// Every SQL type bind time calls `Supported` must have a NULL spelling, so
    /// a parameter that binds can never fail at execute with "SQL type not
    /// implemented". `SQL_SS_UDT` / `SQL_SS_TABLE` are the documented exception:
    /// they need a server type name no describe call reports, and are rejected
    /// at bind instead. Metadata validity is a separate concern, so only the
    /// missing-arm error is checked.
    #[test]
    fn every_supported_sql_type_has_a_typed_null() {
        use crate::api::odbc_types::SQL_SS_TABLE;
        use crate::api::type_rules::{SqlTypeSupport, classify_parameter_sql_type};

        let mut checked = 0;
        for sql_type in -160..=120 {
            if classify_parameter_sql_type(sql_type) != SqlTypeSupport::Supported
                || matches!(sql_type, SQL_SS_UDT | SQL_SS_TABLE)
            {
                continue;
            }
            checked += 1;
            assert!(
                !matches!(
                    typed_null(sql_type, 10, 0),
                    Err(ParamBuildError::UnsupportedSqlType(_))
                ),
                "no typed NULL for supported SQL type {sql_type}"
            );
        }
        assert!(checked > 0, "no supported SQL types found");
    }

    /// The matrix promises at bind time that execute can convert the pairing, so
    /// no pairing it accepts may come back "not implemented" from here.
    #[test]
    fn every_accepted_pairing_is_convertible() {
        let mut buf = [0u8; 32];
        let mut checked = 0;
        for c_type in -160..=120 {
            // Resolved at bind, so it never reaches the matrix.
            if c_type == SQL_C_DEFAULT {
                continue;
            }
            for sql_type in -160..=120 {
                if !is_supported_conversion(c_type, sql_type) {
                    continue;
                }
                let mut ind: SqlLen = 0;
                let mut p = param(c_type, buf.as_mut_ptr() as *mut c_void, &mut ind);
                p.sql_type = sql_type;
                p.column_size = 10;
                if let Err(
                    e @ (ParamBuildError::UnsupportedCType(_)
                    | ParamBuildError::UnsupportedSqlType(_)),
                ) = unsafe { bound_param_to_value(&p) }
                {
                    panic!(
                        "matrix accepts {c_type} -> {sql_type} but the converter returned {e:?}"
                    );
                }
                checked += 1;
            }
        }
        assert!(checked > 0, "matrix iteration found no supported pairings");
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

    /// Non-ASCII must survive the narrow path. `SqlString`'s UTF-8 decode
    /// carries a TODO claiming UTF-16 decode "works better"; this pins that the
    /// UTF-8 tag round-trips so the claim cannot silently become true.
    #[test]
    fn char_round_trips_non_ascii_utf8() {
        let mut buf: Vec<u8> = "caf\u{e9} \u{2615}\0".as_bytes().to_vec();
        let mut ind: SqlLen = SQL_NTS as SqlLen;
        let p = param(SQL_C_CHAR, buf.as_mut_ptr() as *mut c_void, &mut ind);
        let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
        match value {
            SqlType::VarcharMax(Some(s)) => {
                assert_eq!(s.to_utf8_string(), "caf\u{e9} \u{2615}")
            }
            other => panic!("expected VarcharMax(Some), got {other:?}"),
        }
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
        let mut p = param(SQL_C_CHAR, std::ptr::null_mut(), &mut ind);
        p.sql_type = SQL_VARCHAR;
        let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
        assert!(matches!(value, SqlType::VarcharMax(None)));
    }

    #[test]
    fn unsupported_c_type_is_rejected() {
        let mut ind: SqlLen = 4;
        let mut val: f32 = 1.5;
        let p = param(SQL_C_FLOAT, &mut val as *mut f32 as *mut c_void, &mut ind);
        let err = unsafe { bound_param_to_value(&p) }.unwrap_err();
        assert_eq!(err, ParamBuildError::UnsupportedCType(SQL_C_FLOAT));
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
        let mut p = param(SQL_C_WCHAR, std::ptr::null_mut(), &mut ind);
        p.sql_type = SQL_WVARCHAR;
        let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
        assert!(matches!(value, SqlType::NVarcharMax(None)));
    }

    #[test]
    fn null_indicator_unsupported_sql_type_is_rejected() {
        let mut ind: SqlLen = SQL_NULL_DATA;
        let mut p = param(SQL_C_CHAR, std::ptr::null_mut(), &mut ind);
        p.sql_type = SQL_SS_UDT;
        let err = unsafe { bound_param_to_value(&p) }.unwrap_err();
        assert_eq!(err, ParamBuildError::UnsupportedSqlType(SQL_SS_UDT));
    }

    /// A NULL is still declared with its SQL type, so a fixed-length one needs a
    /// real length: `char(0)` is not legal T-SQL. `SQLBindParameter` rejects the
    /// `ColumnSize` before this point, so reaching here means the two checks
    /// disagree.
    #[test]
    fn explicit_char_null_with_zero_column_size_is_rejected() {
        for (c_type, sql_type) in [(SQL_C_CHAR, SQL_CHAR), (SQL_C_WCHAR, SQL_WCHAR)] {
            let mut ind: SqlLen = SQL_NULL_DATA;
            let mut p = param(c_type, std::ptr::null_mut(), &mut ind);
            p.sql_type = sql_type;
            let err = unsafe { bound_param_to_value(&p) }.unwrap_err();
            assert_eq!(
                err,
                ParamBuildError::InvalidParameterSize(0),
                "sql_type {sql_type}"
            );
            assert_eq!(err.diag().state, *b"HY104");
        }
    }

    /// The variable-length types read the same 0 as the `max` spelling.
    #[test]
    fn explicit_varchar_null_with_zero_column_size_is_max() {
        let mut ind: SqlLen = SQL_NULL_DATA;
        let mut p = param(SQL_C_CHAR, std::ptr::null_mut(), &mut ind);
        p.sql_type = SQL_VARCHAR;
        let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
        assert!(matches!(value, SqlType::VarcharMax(None)));

        let mut p = param(SQL_C_WCHAR, std::ptr::null_mut(), &mut ind);
        p.sql_type = SQL_WVARCHAR;
        let (value, _) = unsafe { bound_param_to_value(&p) }.unwrap();
        assert!(matches!(value, SqlType::NVarcharMax(None)));
    }

    /// `SQLBindParameter` screens `ColumnSize` with `parameter_column_size_is_valid`
    /// and the declaration is built here from the same value, so the two encode
    /// the same limits in two places. Anything the bind gate accepts must be
    /// declarable, or the application gets `HY104` from a call it was told had
    /// succeeded.
    ///
    /// Only that direction is asserted. The declaration is deliberately laxer:
    /// an oversized variable-length `ColumnSize` widens to `max` here
    /// (`variable_length`), which the bind gate rejects first, matching
    /// msodbcsql's `CheckSqlPrec`.
    #[test]
    fn every_bind_accepted_column_size_is_declarable() {
        use crate::api::type_rules::parameter_column_size_is_valid;

        for sql_type in [
            SQL_CHAR,
            SQL_WCHAR,
            SQL_BINARY,
            SQL_VARCHAR,
            SQL_WVARCHAR,
            SQL_VARBINARY,
            SQL_LONGVARCHAR,
            SQL_WLONGVARCHAR,
            SQL_LONGVARBINARY,
            SQL_SS_XML,
            SQL_DECIMAL,
            SQL_NUMERIC,
        ] {
            for column_size in [0, 1, 37, 38, 39, 4000, 4001, 8000, 8001] {
                if !parameter_column_size_is_valid(sql_type, column_size) {
                    continue;
                }
                assert!(
                    typed_null(sql_type, column_size, 0).is_ok(),
                    "sql_type {sql_type}: bind accepts ColumnSize {column_size} \
                     but the declaration cannot be built"
                );
            }
        }
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

    /// A defaulted bind of a character SQL type really can describe its buffer
    /// with a character C type, so it reads normally.
    ///
    /// The converse - `decimal`, `numeric`, `sql_variant` and `xml` borrowing
    /// `SQL_C_CHAR`/`SQL_C_WCHAR` and being read as text - is now rejected at
    /// bind by the conversion matrix, and is covered by
    /// `default_bind_rejects_sql_types_whose_default_c_type_is_character`.
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
}
