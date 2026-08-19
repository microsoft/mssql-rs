// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Conversion from a bound application parameter buffer (`BoundParam`) to a
//! TDS RPC parameter (`RpcParameter`).
//!
//! Non-NULL values currently support character C types. A `SQL_NULL_DATA`
//! parameter also supports `SQL_C_DEFAULT`, producing a typed TDS NULL from the
//! SQL type supplied by `SQLBindParameter`.

use std::slice;

use mssql_tds::datatypes::sql_string::{EncodingType, SqlString};
use mssql_tds::datatypes::sqldatatypes::VectorBaseType;
use mssql_tds::datatypes::sqltypes::SqlType;
use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};

use crate::api::odbc_types::{
    SQL_BIGINT, SQL_BINARY, SQL_BIT, SQL_C_CHAR, SQL_C_DEFAULT, SQL_C_LONG, SQL_C_WCHAR, SQL_CHAR,
    SQL_DATA_AT_EXEC, SQL_DECIMAL, SQL_DEFAULT_PARAM, SQL_DOUBLE, SQL_FLOAT, SQL_GUID, SQL_INTEGER,
    SQL_LEN_DATA_AT_EXEC_OFFSET, SQL_LONGVARBINARY, SQL_LONGVARCHAR, SQL_NTS, SQL_NULL_DATA,
    SQL_NUMERIC, SQL_REAL, SQL_SMALLINT, SQL_SS_TIME2, SQL_SS_TIMESTAMPOFFSET, SQL_SS_VARIANT,
    SQL_SS_VECTOR, SQL_SS_XML, SQL_TINYINT, SQL_TYPE_DATE, SQL_TYPE_TIME, SQL_TYPE_TIMESTAMP,
    SQL_VARBINARY, SQL_VARCHAR, SQL_WCHAR, SQL_WLONGVARCHAR, SQL_WVARCHAR, SqlLen, SqlSmallInt,
};
use crate::api::sqlstate::ERR_INVALID_STRING_OR_BUFFER_LENGTH;
use crate::params::BoundParam;

/// Why a bound parameter could not be converted.
///
/// The "not yet implemented" variants post `HYC00` via [`message`]; a bad
/// indicator posts the canonical `HY090` diagnostic
/// (`ERR_INVALID_STRING_OR_BUFFER_LENGTH`).
///
/// [`message`]: ParamConvError::message
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParamConvError {
    /// The application's C type is not supported in Phase 1.
    UnsupportedCType(SqlSmallInt),
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
            Self::DataAtExecUnsupported => "Data-at-execution parameters not yet implemented",
            Self::DefaultParamUnsupported => "Default parameters not yet implemented",
            Self::InvalidLength(_) => ERR_INVALID_STRING_OR_BUFFER_LENGTH.text,
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
) -> Result<RpcParameter, ParamConvError> {
    let value = unsafe { bound_param_to_value(param) }?;
    let parameter = RpcParameter::new(Some(name), StatusFlags::NONE, value);
    if param.c_type == SQL_C_DEFAULT {
        Ok(parameter.with_declaration(default_sql_declaration(
            param.sql_type,
            param.column_size,
            param.decimal_digits,
        )?))
    } else {
        Ok(parameter)
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

    if let Some(ind) = indicator {
        if ind == SQL_NULL_DATA {
            return null_value(
                param.c_type,
                param.sql_type,
                param.column_size,
                param.decimal_digits,
            );
        }
        if ind == SQL_DEFAULT_PARAM {
            return Err(ParamConvError::DefaultParamUnsupported);
        }
        if ind == SQL_DATA_AT_EXEC || ind <= SQL_LEN_DATA_AT_EXEC_OFFSET {
            return Err(ParamConvError::DataAtExecUnsupported);
        }
        // Any remaining negative indicator  is invalid for an input parameter
        if ind < 0 && ind != SQL_NTS as SqlLen {
            return Err(ParamConvError::InvalidLength(ind));
        }
    }

    // For string C types a null indicator pointer means "null-terminated".
    let len_spec = indicator.unwrap_or(SQL_NTS as SqlLen);

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
        other => return Err(ParamConvError::UnsupportedCType(other)),
    };

    Ok(value)
}

/// Typed NULL for the supported C types.
fn null_value(
    c_type: SqlSmallInt,
    sql_type: SqlSmallInt,
    column_size: usize,
    decimal_digits: SqlSmallInt,
) -> Result<SqlType, ParamConvError> {
    match c_type {
        SQL_C_CHAR => Ok(SqlType::VarcharMax(None)),
        SQL_C_WCHAR => Ok(SqlType::NVarcharMax(None)),
        SQL_C_DEFAULT => default_typed_null(sql_type, column_size, decimal_digits),
        other => Err(ParamConvError::UnsupportedCType(other)),
    }
}

fn default_typed_null(
    sql_type: SqlSmallInt,
    column_size: usize,
    decimal_digits: SqlSmallInt,
) -> Result<SqlType, ParamConvError> {
    let declared_length = u16::try_from(column_size).unwrap_or(u16::MAX);
    let value = match sql_type {
        SQL_BIT => SqlType::Bit(None),
        SQL_TINYINT => SqlType::TinyInt(None),
        SQL_SMALLINT => SqlType::SmallInt(None),
        SQL_INTEGER => SqlType::Int(None),
        SQL_BIGINT => SqlType::BigInt(None),
        SQL_REAL => SqlType::Real(None),
        SQL_FLOAT | SQL_DOUBLE => SqlType::Float(None),
        SQL_DECIMAL => SqlType::Decimal(None),
        SQL_NUMERIC => SqlType::Numeric(None),
        SQL_CHAR => SqlType::Char(None, declared_length),
        SQL_VARCHAR if column_size == 0 => SqlType::VarcharMax(None),
        SQL_VARCHAR => SqlType::Varchar(None, declared_length),
        SQL_LONGVARCHAR => SqlType::Text(None),
        SQL_WCHAR => SqlType::NChar(None, declared_length),
        SQL_WVARCHAR if column_size == 0 => SqlType::NVarcharMax(None),
        SQL_WVARCHAR => SqlType::NVarchar(None, declared_length),
        SQL_WLONGVARCHAR => SqlType::NText(None),
        SQL_BINARY => SqlType::Binary(None, declared_length),
        SQL_VARBINARY if column_size == 0 => SqlType::VarBinaryMax(None),
        SQL_VARBINARY => SqlType::VarBinary(None, declared_length),
        SQL_LONGVARBINARY => SqlType::VarBinaryMax(None),
        SQL_GUID => SqlType::Uuid(None),
        SQL_TYPE_DATE => SqlType::Date(None),
        SQL_TYPE_TIME | SQL_SS_TIME2 => SqlType::Time(None),
        SQL_TYPE_TIMESTAMP => SqlType::DateTime2(None),
        SQL_SS_TIMESTAMPOFFSET => SqlType::DateTimeOffset(None),
        SQL_SS_XML => SqlType::Xml(None),
        SQL_SS_VARIANT => SqlType::Variant(Box::new(SqlType::Varchar(None, 1))),
        SQL_SS_VECTOR => {
            let (dimensions, base_type) = vector_metadata(column_size, decimal_digits)?;
            SqlType::Vector(None, dimensions, base_type)
        }
        _ => return Err(ParamConvError::UnsupportedCType(SQL_C_DEFAULT)),
    };
    Ok(value)
}

fn default_sql_declaration(
    sql_type: SqlSmallInt,
    column_size: usize,
    decimal_digits: SqlSmallInt,
) -> Result<String, ParamConvError> {
    let sized = |name: &str| {
        if column_size == 0 {
            format!("{name}(max)")
        } else {
            format!("{name}({column_size})")
        }
    };
    let declaration = match sql_type {
        SQL_BIT => "bit".to_string(),
        SQL_TINYINT => "tinyint".to_string(),
        SQL_SMALLINT => "smallint".to_string(),
        SQL_INTEGER => "int".to_string(),
        SQL_BIGINT => "bigint".to_string(),
        SQL_REAL => "real".to_string(),
        SQL_FLOAT | SQL_DOUBLE => "float".to_string(),
        SQL_DECIMAL => format!("decimal({column_size},{decimal_digits})"),
        SQL_NUMERIC => format!("numeric({column_size},{decimal_digits})"),
        SQL_CHAR => format!("char({column_size})"),
        SQL_VARCHAR => sized("varchar"),
        SQL_LONGVARCHAR => "text".to_string(),
        SQL_WCHAR => format!("nchar({column_size})"),
        SQL_WVARCHAR => sized("nvarchar"),
        SQL_WLONGVARCHAR => "ntext".to_string(),
        SQL_BINARY => format!("binary({column_size})"),
        SQL_VARBINARY => sized("varbinary"),
        SQL_LONGVARBINARY => "varbinary(max)".to_string(),
        SQL_GUID => "uniqueidentifier".to_string(),
        SQL_TYPE_DATE => "date".to_string(),
        SQL_TYPE_TIME | SQL_SS_TIME2 => format!("time({decimal_digits})"),
        SQL_TYPE_TIMESTAMP => format!("datetime2({decimal_digits})"),
        SQL_SS_TIMESTAMPOFFSET => format!("datetimeoffset({decimal_digits})"),
        SQL_SS_XML => "xml".to_string(),
        SQL_SS_VARIANT => "sql_variant".to_string(),
        SQL_SS_VECTOR => {
            let (dimensions, _) = vector_metadata(column_size, decimal_digits)?;
            format!("vector({dimensions})")
        }
        _ => return Err(ParamConvError::UnsupportedCType(SQL_C_DEFAULT)),
    };
    Ok(declaration)
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
            | SQL_SS_VARIANT
            | SQL_SS_VECTOR
            | SQL_SS_XML
    )
}

#[repr(C)]
struct SqlSsVectorLayout {
    dimension: SqlSmallInt,
    vector_type: i32,
    data: *mut std::ffi::c_void,
}

fn vector_metadata(
    column_size: usize,
    base_type: SqlSmallInt,
) -> Result<(u16, VectorBaseType), ParamConvError> {
    let payload_size = column_size
        .checked_sub(std::mem::size_of::<SqlSsVectorLayout>())
        .filter(|size| size % std::mem::size_of::<f32>() == 0)
        .ok_or(ParamConvError::UnsupportedCType(SQL_C_DEFAULT))?;
    let dimensions = u16::try_from(payload_size / std::mem::size_of::<f32>())
        .map_err(|_| ParamConvError::UnsupportedCType(SQL_C_DEFAULT))?;
    let base_type = match base_type {
        0 => VectorBaseType::Float32,
        1 => VectorBaseType::Float16,
        _ => return Err(ParamConvError::UnsupportedCType(SQL_C_DEFAULT)),
    };
    Ok((dimensions, base_type))
}

/// Known ODBC C type identifiers accepted at bind time.
pub(crate) fn is_valid_c_type(c_type: SqlSmallInt) -> bool {
    matches!(
        c_type,
        SQL_C_CHAR | SQL_C_WCHAR | SQL_C_LONG | SQL_C_DEFAULT
    )
}

/// Whether the C type → SQL type conversion is supported. Phase 1 only allows
/// same-family character conversions: `SQL_C_CHAR` → narrow character SQL types
/// (`CHAR`/`VARCHAR`/`LONGVARCHAR`) and `SQL_C_WCHAR` → the wide character SQL
/// types (`WCHAR`/`WVARCHAR`/`WLONGVARCHAR`). Every other pairing is rejected
/// (`07006`).
pub(crate) fn is_valid_conversion(c_type: SqlSmallInt, sql_type: SqlSmallInt) -> bool {
    match c_type {
        SQL_C_CHAR => matches!(sql_type, SQL_CHAR | SQL_VARCHAR | SQL_LONGVARCHAR),
        SQL_C_WCHAR => matches!(sql_type, SQL_WCHAR | SQL_WVARCHAR | SQL_WLONGVARCHAR),
        SQL_C_DEFAULT => true,
        _ => false,
    }
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
    use crate::api::odbc_types::{SQL_C_LONG, SQL_NO_TOTAL, SQL_PARAM_INPUT};
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

    #[test]
    fn char_nts_becomes_varchar() {
        let mut buf: Vec<u8> = b"hello\0".to_vec();
        let mut ind: SqlLen = SQL_NTS as SqlLen;
        let p = param(SQL_C_CHAR, buf.as_mut_ptr() as *mut c_void, &mut ind);
        let value = unsafe { bound_param_to_value(&p) }.unwrap();
        match value {
            SqlType::VarcharMax(Some(s)) => assert_eq!(s.to_utf8_string(), "hello"),
            other => panic!("expected VarcharMax(Some), got {other:?}"),
        }
    }

    #[test]
    fn wchar_explicit_length_becomes_nvarchar() {
        let mut buf: Vec<u16> = "hi".encode_utf16().collect();
        let mut ind: SqlLen = (buf.len() * 2) as SqlLen;
        let p = param(SQL_C_WCHAR, buf.as_mut_ptr() as *mut c_void, &mut ind);
        let value = unsafe { bound_param_to_value(&p) }.unwrap();
        match value {
            SqlType::NVarcharMax(Some(s)) => assert_eq!(s.to_utf8_string(), "hi"),
            other => panic!("expected NVarcharMax(Some), got {other:?}"),
        }
    }

    #[test]
    fn null_indicator_yields_typed_null() {
        let mut ind: SqlLen = SQL_NULL_DATA;
        let p = param(SQL_C_CHAR, std::ptr::null_mut(), &mut ind);
        let value = unsafe { bound_param_to_value(&p) }.unwrap();
        assert!(matches!(value, SqlType::VarcharMax(None)));
    }

    #[test]
    fn unsupported_c_type_is_rejected() {
        let mut ind: SqlLen = 4;
        let mut val: i32 = 7;
        let p = param(SQL_C_LONG, &mut val as *mut i32 as *mut c_void, &mut ind);
        let err = unsafe { bound_param_to_value(&p) }.unwrap_err();
        assert_eq!(err, ParamConvError::UnsupportedCType(SQL_C_LONG));
    }

    #[test]
    fn data_at_exec_is_rejected() {
        let mut ind: SqlLen = SQL_DATA_AT_EXEC;
        let p = param(SQL_C_CHAR, std::ptr::null_mut(), &mut ind);
        let err = unsafe { bound_param_to_value(&p) }.unwrap_err();
        assert_eq!(err, ParamConvError::DataAtExecUnsupported);
    }

    #[test]
    fn invalid_indicator_is_rejected() {
        let mut ind: SqlLen = SQL_NO_TOTAL;
        let p = param(SQL_C_CHAR, std::ptr::null_mut(), &mut ind);
        let err = unsafe { bound_param_to_value(&p) }.unwrap_err();
        assert_eq!(err, ParamConvError::InvalidLength(SQL_NO_TOTAL));
    }

    #[test]
    fn conversion_allows_same_family_only() {
        assert!(is_valid_conversion(SQL_C_CHAR, SQL_VARCHAR));
        assert!(is_valid_conversion(SQL_C_WCHAR, SQL_WVARCHAR));
        // Cross-family, non-character, and unsupported C types are rejected.
        assert!(!is_valid_conversion(SQL_C_CHAR, SQL_WVARCHAR));
        assert!(!is_valid_conversion(SQL_C_WCHAR, SQL_VARCHAR));
        assert!(!is_valid_conversion(SQL_C_CHAR, SQL_INTEGER));
        assert!(!is_valid_conversion(SQL_C_LONG, SQL_INTEGER));
        assert!(is_valid_conversion(SQL_C_DEFAULT, SQL_INTEGER));
    }

    #[test]
    fn default_param_indicator_is_rejected() {
        let mut ind: SqlLen = SQL_DEFAULT_PARAM;
        let p = param(SQL_C_CHAR, std::ptr::null_mut(), &mut ind);
        let err = unsafe { bound_param_to_value(&p) }.unwrap_err();
        assert_eq!(err, ParamConvError::DefaultParamUnsupported);
    }

    #[test]
    fn null_indicator_wchar_yields_typed_null() {
        let mut ind: SqlLen = SQL_NULL_DATA;
        let p = param(SQL_C_WCHAR, std::ptr::null_mut(), &mut ind);
        let value = unsafe { bound_param_to_value(&p) }.unwrap();
        assert!(matches!(value, SqlType::NVarcharMax(None)));
    }

    #[test]
    fn null_indicator_unsupported_c_type_is_rejected() {
        let mut ind: SqlLen = SQL_NULL_DATA;
        let p = param(SQL_C_LONG, std::ptr::null_mut(), &mut ind);
        let err = unsafe { bound_param_to_value(&p) }.unwrap_err();
        assert_eq!(err, ParamConvError::UnsupportedCType(SQL_C_LONG));
    }

    #[test]
    fn default_null_uses_described_sql_type() {
        let mut ind: SqlLen = SQL_NULL_DATA;
        let mut p = param(SQL_C_DEFAULT, std::ptr::null_mut(), &mut ind);
        p.sql_type = SQL_INTEGER;
        p.column_size = 10;
        let value = unsafe { bound_param_to_value(&p) }.unwrap();
        assert!(matches!(value, SqlType::Int(None)));

        p.sql_type = SQL_WVARCHAR;
        p.column_size = 40;
        let value = unsafe { bound_param_to_value(&p) }.unwrap();
        assert!(matches!(value, SqlType::NVarchar(None, 40)));

        assert_eq!(
            default_sql_declaration(SQL_DECIMAL, 12, 3).unwrap(),
            "decimal(12,3)"
        );
        assert_eq!(
            default_sql_declaration(SQL_SS_TIME2, 12, 4).unwrap(),
            "time(4)"
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
