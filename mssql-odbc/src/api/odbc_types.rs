// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ODBC C type aliases matching the ODBC 3.x specification.
#![allow(dead_code)]

use std::ffi::c_void;

pub type SqlSmallInt = i16;
pub type SqlUSmallInt = u16;
pub type SqlInteger = i32;
pub type SqlLen = isize;
pub type SqlULen = usize;
pub type SqlReturn = SqlSmallInt;
pub type SqlHandle = *mut c_void;
pub type SqlHWnd = *mut c_void;
pub type SqlPointer = *mut c_void;
/// UTF-16 code unit. ODBC `SQLWCHAR` is 16-bit on every supported platform
pub type SqlWChar = u16;

/// Length of a SQLSTATE string in characters (5 + NUL written separately).
pub const SQL_SQLSTATE_SIZE: usize = 5;

// Return codes
pub const SQL_SUCCESS: SqlReturn = 0;
pub const SQL_SUCCESS_WITH_INFO: SqlReturn = 1;
pub const SQL_NO_DATA: SqlReturn = 100;
pub const SQL_PARAM_DATA_AVAILABLE: SqlReturn = 101;
pub const SQL_ERROR: SqlReturn = -1;
pub const SQL_INVALID_HANDLE: SqlReturn = -2;
pub const SQL_STILL_EXECUTING: SqlReturn = 2;
pub const SQL_NEED_DATA: SqlReturn = 99;

// Null handle sentinel
pub const SQL_NULL_HANDLE: SqlHandle = std::ptr::null_mut();

// Handle type constants
pub const SQL_HANDLE_ENV: SqlSmallInt = 1;
pub const SQL_HANDLE_DBC: SqlSmallInt = 2;
pub const SQL_HANDLE_STMT: SqlSmallInt = 3;
pub const SQL_HANDLE_DESC: SqlSmallInt = 4;
// Internal handle type used by Driver Manager for connection pool awareness.
// Applications should not use this directly.
pub const SQL_HANDLE_DBC_INFO_TOKEN: SqlSmallInt = 6;

// Environment attribute identifiers (SQLSetEnvAttr / SQLGetEnvAttr).
pub const SQL_ATTR_ODBC_VERSION: SqlInteger = 200;

// SQL_ATTR_ODBC_VERSION values.
pub const SQL_OV_ODBC2: u32 = 2;
pub const SQL_OV_ODBC3: u32 = 3;
pub const SQL_OV_ODBC3_80: u32 = 380;

// Connection attribute identifiers (SQLSetConnectAttr / SQLGetConnectAttr).
// msodbcsql-specific: pre-connect Entra access token. `value_ptr` points to an
// ACCESSTOKEN struct: a 4-byte little-endian length followed by that many bytes
// of the UTF-16-LE-encoded token.
pub const SQL_COPT_SS_ACCESS_TOKEN: SqlInteger = 1256;

// Standard ODBC connection attributes the Driver Manager commonly sets before
// connecting. Accepted (currently ignored) so the DM handshake is not broken.
pub const SQL_ATTR_ACCESS_MODE: SqlInteger = 101;
pub const SQL_ATTR_LOGIN_TIMEOUT: SqlInteger = 103;
pub const SQL_ATTR_PACKET_SIZE: SqlInteger = 112;
pub const SQL_ATTR_CONNECTION_TIMEOUT: SqlInteger = 113;

// Sentinel `StringLength` meaning "the value is a pointer" (ODBC).
pub const SQL_IS_POINTER: SqlInteger = -4;

// DriverCompletion constants for SQLDriverConnect
pub const SQL_DRIVER_NOPROMPT: SqlUSmallInt = 0;
pub const SQL_DRIVER_COMPLETE: SqlUSmallInt = 1;
pub const SQL_DRIVER_PROMPT: SqlUSmallInt = 2;
pub const SQL_DRIVER_COMPLETE_REQUIRED: SqlUSmallInt = 3;

// SQLFreeStmt option constants
pub const SQL_CLOSE: SqlUSmallInt = 0;
pub const SQL_DROP: SqlUSmallInt = 1;
pub const SQL_UNBIND: SqlUSmallInt = 2;
pub const SQL_RESET_PARAMS: SqlUSmallInt = 3;

// Null-terminated string sentinel
pub const SQL_NTS: SqlSmallInt = -3;

// ODBC-SQL-type identifiers.
pub const SQL_UNKNOWN_TYPE: SqlSmallInt = 0;
pub const SQL_CHAR: SqlSmallInt = 1;
pub const SQL_NUMERIC: SqlSmallInt = 2;
pub const SQL_DECIMAL: SqlSmallInt = 3;
pub const SQL_INTEGER: SqlSmallInt = 4;
pub const SQL_SMALLINT: SqlSmallInt = 5;
pub const SQL_FLOAT: SqlSmallInt = 6;
pub const SQL_REAL: SqlSmallInt = 7;
pub const SQL_DOUBLE: SqlSmallInt = 8;
pub const SQL_DATETIME: SqlSmallInt = 9;
pub const SQL_VARCHAR: SqlSmallInt = 12;
pub const SQL_TIMESTAMP: SqlSmallInt = 11;
pub const SQL_TYPE_DATE: SqlSmallInt = 91;
pub const SQL_TYPE_TIME: SqlSmallInt = 92;
pub const SQL_TYPE_TIMESTAMP: SqlSmallInt = 93;
pub const SQL_LONGVARCHAR: SqlSmallInt = -1;
pub const SQL_BINARY: SqlSmallInt = -2;
pub const SQL_VARBINARY: SqlSmallInt = -3;
pub const SQL_LONGVARBINARY: SqlSmallInt = -4;
pub const SQL_BIGINT: SqlSmallInt = -5;
pub const SQL_TINYINT: SqlSmallInt = -6;
pub const SQL_BIT: SqlSmallInt = -7;
pub const SQL_WCHAR: SqlSmallInt = -8;
pub const SQL_WVARCHAR: SqlSmallInt = -9;
pub const SQL_WLONGVARCHAR: SqlSmallInt = -10;
pub const SQL_GUID: SqlSmallInt = -11;

// SQL Server-specific ODBC-SQL-type identifiers (driver extensions).
pub const SQL_SS_TIME2: SqlSmallInt = -154;
pub const SQL_SS_TIMESTAMPOFFSET: SqlSmallInt = -155;

// ODBC C types
pub const SQL_C_CHAR: SqlSmallInt = 1;
pub const SQL_C_WCHAR: SqlSmallInt = -8;
pub const SQL_C_LONG: SqlSmallInt = 4;
/// `SQL_C_DEFAULT` — bind using the C type that maps to the SQL type.
pub const SQL_C_DEFAULT: SqlSmallInt = 99;

// SQLBindParameter InputOutputType values.
pub const SQL_PARAM_TYPE_UNKNOWN: SqlSmallInt = 0;
pub const SQL_PARAM_INPUT: SqlSmallInt = 1;
pub const SQL_PARAM_INPUT_OUTPUT: SqlSmallInt = 2;
pub const SQL_PARAM_OUTPUT: SqlSmallInt = 4;

// Values of NULLABLE field in descriptor
pub const SQL_NO_NULLS: SqlSmallInt = 0;
pub const SQL_NULLABLE: SqlSmallInt = 1;

// Diagnostic field identifiers (SQLGetDiagField)
pub const SQL_DIAG_NUMBER: SqlSmallInt = 2;
pub const SQL_DIAG_SQLSTATE: SqlSmallInt = 4;
pub const SQL_DIAG_NATIVE: SqlSmallInt = 5;
pub const SQL_DIAG_MESSAGE_TEXT: SqlSmallInt = 6;

// Special length/indicator constants.
pub const SQL_NULL_DATA: SqlLen = -1;
pub const SQL_DATA_AT_EXEC: SqlLen = -2;

// SQLBindParameter extensions
pub const SQL_DEFAULT_PARAM: SqlLen = -5;
pub const SQL_IGNORE: SqlLen = -6;

pub const fn sql_len_data_at_exec(length: SqlLen) -> SqlLen {
    -length + SQL_LEN_DATA_AT_EXEC_OFFSET
}
/// Indicator values at or below this offset encode a data-at-execution length
/// via the `SQL_LEN_DATA_AT_EXEC(n)` macro.
pub const SQL_LEN_DATA_AT_EXEC_OFFSET: SqlLen = -100;
