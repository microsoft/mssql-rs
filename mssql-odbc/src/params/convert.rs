// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Conversion from a bound application parameter buffer (`BoundParam`) to a
//! TDS RPC parameter (`RpcParameter`).
//!
//! Phase 1 mirrors `SQLGetData`'s supported C types: only `SQL_C_CHAR`
//! (→ `varchar`) and `SQL_C_WCHAR` (→ `nvarchar`). Every other C type, plus
//! data-at-execution and default parameters, is rejected with `HYC00`.

use std::slice;

use mssql_tds::datatypes::sql_string::{EncodingType, SqlString};
use mssql_tds::datatypes::sqltypes::SqlType;
use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};

use crate::api::odbc_types::{
    SQL_BIGINT, SQL_BINARY, SQL_BIT, SQL_C_CHAR, SQL_C_DEFAULT, SQL_C_LONG, SQL_C_WCHAR, SQL_CHAR,
    SQL_DATA_AT_EXEC, SQL_DECIMAL, SQL_DEFAULT_PARAM, SQL_DOUBLE, SQL_FLOAT, SQL_GUID, SQL_INTEGER,
    SQL_LEN_DATA_AT_EXEC_OFFSET, SQL_LONGVARBINARY, SQL_LONGVARCHAR, SQL_NTS, SQL_NULL_DATA,
    SQL_NUMERIC, SQL_REAL, SQL_SMALLINT, SQL_SS_TIME2, SQL_SS_TIMESTAMPOFFSET, SQL_TINYINT,
    SQL_TYPE_DATE, SQL_TYPE_TIME, SQL_TYPE_TIMESTAMP, SQL_VARBINARY, SQL_VARCHAR, SQL_WCHAR,
    SQL_WLONGVARCHAR, SQL_WVARCHAR, SqlLen, SqlSmallInt,
};
use crate::params::BoundParam;

/// Why a bound parameter could not be converted. All map to SQLSTATE `HYC00`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ParamConvError {
    /// The application's C type is not supported in Phase 1.
    UnsupportedCType(SqlSmallInt),
    /// The parameter uses data-at-execution (`SQLPutData`).
    DataAtExecUnsupported,
    /// The parameter requested its default value.
    DefaultParamUnsupported,
}

impl ParamConvError {
    pub(crate) fn message(self) -> &'static str {
        match self {
            Self::UnsupportedCType(_) => "Parameter C type not yet implemented",
            Self::DataAtExecUnsupported => "Data-at-execution parameters not yet implemented",
            Self::DefaultParamUnsupported => "Default parameters not yet implemented",
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
    Ok(RpcParameter::new(Some(name), StatusFlags::NONE, value))
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
            return null_value(param.c_type);
        }
        if ind == SQL_DEFAULT_PARAM {
            return Err(ParamConvError::DefaultParamUnsupported);
        }
        if ind == SQL_DATA_AT_EXEC || ind <= SQL_LEN_DATA_AT_EXEC_OFFSET {
            return Err(ParamConvError::DataAtExecUnsupported);
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
fn null_value(c_type: SqlSmallInt) -> Result<SqlType, ParamConvError> {
    match c_type {
        SQL_C_CHAR => Ok(SqlType::VarcharMax(None)),
        SQL_C_WCHAR => Ok(SqlType::NVarcharMax(None)),
        other => Err(ParamConvError::UnsupportedCType(other)),
    }
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
    use crate::api::odbc_types::{SQL_C_LONG, SQL_PARAM_INPUT};
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
    fn conversion_allows_same_family_only() {
        assert!(is_valid_conversion(SQL_C_CHAR, SQL_VARCHAR));
        assert!(is_valid_conversion(SQL_C_WCHAR, SQL_WVARCHAR));
        // Cross-family, non-character, and unsupported C types are rejected.
        assert!(!is_valid_conversion(SQL_C_CHAR, SQL_WVARCHAR));
        assert!(!is_valid_conversion(SQL_C_WCHAR, SQL_VARCHAR));
        assert!(!is_valid_conversion(SQL_C_CHAR, SQL_INTEGER));
        assert!(!is_valid_conversion(SQL_C_LONG, SQL_INTEGER));
    }
}
