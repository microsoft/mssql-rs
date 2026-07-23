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
pub const SQL_ATTR_ANSI_APP: SqlInteger = 115;

// Sentinel `StringLength` meaning "the value is a pointer" (ODBC).
pub const SQL_IS_POINTER: SqlInteger = -4;

// Four types of descriptor handles
pub const SQL_ATTR_APP_ROW_DESC: SqlInteger = 10010;
pub const SQL_ATTR_APP_PARAM_DESC: SqlInteger = 10011;
pub const SQL_ATTR_IMP_ROW_DESC: SqlInteger = 10012;
pub const SQL_ATTR_IMP_PARAM_DESC: SqlInteger = 10013;

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

pub const SQL_FALSE: SqlUSmallInt = 0;
pub const SQL_TRUE: SqlUSmallInt = 1;

// SQLGetFunctions selectors.
pub const SQL_API_ALL_FUNCTIONS: SqlUSmallInt = 0;
pub const SQL_API_ODBC3_ALL_FUNCTIONS: SqlUSmallInt = 999;
pub const SQL_API_ALL_FUNCTIONS_SIZE: usize = 100;
pub const SQL_API_ODBC3_ALL_FUNCTIONS_SIZE: usize = 250;

// Function identifiers (SQLGetFunctions / SQL_FUNC_EXISTS bitmap ids).
pub const SQL_API_SQLCONNECT: SqlUSmallInt = 7;
pub const SQL_API_SQLCANCEL: SqlUSmallInt = 5;
pub const SQL_API_SQLDESCRIBECOL: SqlUSmallInt = 8;
pub const SQL_API_SQLDISCONNECT: SqlUSmallInt = 9;
pub const SQL_API_SQLEXECDIRECT: SqlUSmallInt = 11;
pub const SQL_API_SQLEXECUTE: SqlUSmallInt = 12;
pub const SQL_API_SQLFETCH: SqlUSmallInt = 13;
pub const SQL_API_SQLFREESTMT: SqlUSmallInt = 16;
pub const SQL_API_SQLNUMRESULTCOLS: SqlUSmallInt = 18;
pub const SQL_API_SQLPREPARE: SqlUSmallInt = 19;
pub const SQL_API_SQLROWCOUNT: SqlUSmallInt = 20;
pub const SQL_API_SQLDRIVERCONNECT: SqlUSmallInt = 41;
pub const SQL_API_SQLGETDATA: SqlUSmallInt = 43;
pub const SQL_API_SQLGETFUNCTIONS: SqlUSmallInt = 44;
pub const SQL_API_SQLGETINFO: SqlUSmallInt = 45;
pub const SQL_API_SQLBINDPARAMETER: SqlUSmallInt = 72;
pub const SQL_API_SQLMORERESULTS: SqlUSmallInt = 61;
pub const SQL_API_SQLALLOCHANDLE: SqlUSmallInt = 1001;
pub const SQL_API_SQLCLOSECURSOR: SqlUSmallInt = 1003;
pub const SQL_API_SQLFREEHANDLE: SqlUSmallInt = 1006;
pub const SQL_API_SQLGETDIAGFIELD: SqlUSmallInt = 1010;
pub const SQL_API_SQLGETDIAGREC: SqlUSmallInt = 1011;
pub const SQL_API_SQLGETENVATTR: SqlUSmallInt = 1012;
pub const SQL_API_SQLGETSTMTATTR: SqlUSmallInt = 1014;
pub const SQL_API_SQLSETCONNECTATTR: SqlUSmallInt = 1016;
pub const SQL_API_SQLSETENVATTR: SqlUSmallInt = 1019;

// SQLGetInfo info-type identifiers.
pub const SQL_MAX_DRIVER_CONNECTIONS: SqlUSmallInt = 0;
pub const SQL_ACTIVE_STATEMENTS: SqlUSmallInt = 1;
pub const SQL_DRIVER_NAME: SqlUSmallInt = 6;
pub const SQL_DRIVER_VER: SqlUSmallInt = 7;
pub const SQL_ODBC_API_CONFORMANCE: SqlUSmallInt = 9;
pub const SQL_ODBC_VER: SqlUSmallInt = 10;
pub const SQL_ODBC_SQL_CONFORMANCE: SqlUSmallInt = 15;
pub const SQL_DBMS_NAME: SqlUSmallInt = 17;
pub const SQL_DBMS_VER: SqlUSmallInt = 18;
pub const SQL_CURSOR_COMMIT_BEHAVIOR: SqlUSmallInt = 23;
pub const SQL_CURSOR_ROLLBACK_BEHAVIOR: SqlUSmallInt = 24;
pub const SQL_IDENTIFIER_QUOTE_CHAR: SqlUSmallInt = 29;
pub const SQL_DRIVER_ODBC_VER: SqlUSmallInt = 77;
pub const SQL_GETDATA_EXTENSIONS: SqlUSmallInt = 81;
pub const SQL_NEED_LONG_DATA_LEN: SqlUSmallInt = 111;
pub const SQL_DM_VER: SqlUSmallInt = 171;
pub const SQL_ASYNC_DBC_FUNCTIONS: SqlUSmallInt = 10023;
pub const SQL_ASYNC_NOTIFICATION: SqlUSmallInt = 10025;

// SQLGetInfo return values.
pub const SQL_OAC_LEVEL2: u16 = 0x0002;
pub const SQL_OSC_CORE: u16 = 0x0001;
pub const SQL_CB_CLOSE: u16 = 1;
pub const SQL_GD_ANY_COLUMN: u32 = 0x00000001;
pub const SQL_GD_ANY_ORDER: u32 = 0x00000002;
pub const SQL_ASYNC_DBC_NOT_CAPABLE: u32 = 0x00000000;
pub const SQL_ASYNC_NOTIFICATION_NOT_CAPABLE: u32 = 0x00000000;

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
pub const SQL_DIAG_CLASS_ORIGIN: SqlSmallInt = 8;
pub const SQL_DIAG_SUBCLASS_ORIGIN: SqlSmallInt = 9;
pub const SQL_DIAG_CONNECTION_NAME: SqlSmallInt = 10;
pub const SQL_DIAG_SERVER_NAME: SqlSmallInt = 11;
pub const SQL_DIAG_DYNAMIC_FUNCTION_CODE: SqlSmallInt = 12;

// Dynamic-function-code value: statement type is unknown/unclassified.
pub const SQL_DIAG_UNKNOWN_STATEMENT: SqlInteger = 0;

// Special length/indicator constants.
pub const SQL_NULL_DATA: SqlLen = -1;
pub const SQL_DATA_AT_EXEC: SqlLen = -2;
/// Driver-supplied "length unknown" indicator; never a valid application input
/// length.
pub const SQL_NO_TOTAL: SqlLen = -4;

// SQLBindParameter extensions
pub const SQL_DEFAULT_PARAM: SqlLen = -5;
pub const SQL_IGNORE: SqlLen = -6;

pub const fn sql_len_data_at_exec(length: SqlLen) -> SqlLen {
    -length + SQL_LEN_DATA_AT_EXEC_OFFSET
}
/// Indicator values at or below this offset encode a data-at-execution length
/// via the `SQL_LEN_DATA_AT_EXEC(n)` macro.
pub const SQL_LEN_DATA_AT_EXEC_OFFSET: SqlLen = -100;
