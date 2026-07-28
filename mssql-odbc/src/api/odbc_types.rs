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
/// ODBC `SQLUINTEGER` — unsigned 32-bit integer, used in C interop structs.
pub type SqlUInteger = u32;

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

// ---- SQL Server-specific ODBC-SQL-type identifiers (driver extensions) ------
// The `SS_TIME2`/`SS_TIMESTAMPOFFSET` ids are declared above; these complete the
// set used by the fetch path.
pub const SQL_SS_VARIANT: SqlSmallInt = -150;
pub const SQL_SS_UDT: SqlSmallInt = -151;
pub const SQL_SS_XML: SqlSmallInt = -152;

// ---- Additional ODBC C type identifiers (SQLBindCol / SQLGetData) -----------
// Signed/unsigned integer C types are the base numeric type id plus an offset,
// exactly as defined in the ODBC headers.
pub const SQL_SIGNED_OFFSET: SqlSmallInt = -20;
pub const SQL_UNSIGNED_OFFSET: SqlSmallInt = -22;

pub const SQL_C_SHORT: SqlSmallInt = SQL_SMALLINT; // 5
pub const SQL_C_SSHORT: SqlSmallInt = SQL_C_SHORT + SQL_SIGNED_OFFSET; // -15
pub const SQL_C_USHORT: SqlSmallInt = SQL_C_SHORT + SQL_UNSIGNED_OFFSET; // -17
pub const SQL_C_SLONG: SqlSmallInt = SQL_C_LONG + SQL_SIGNED_OFFSET; // -16
pub const SQL_C_ULONG: SqlSmallInt = SQL_C_LONG + SQL_UNSIGNED_OFFSET; // -18
pub const SQL_C_TINYINT: SqlSmallInt = SQL_TINYINT; // -6
pub const SQL_C_STINYINT: SqlSmallInt = SQL_TINYINT + SQL_SIGNED_OFFSET; // -26
pub const SQL_C_UTINYINT: SqlSmallInt = SQL_TINYINT + SQL_UNSIGNED_OFFSET; // -28
pub const SQL_C_SBIGINT: SqlSmallInt = SQL_BIGINT + SQL_SIGNED_OFFSET; // -25
pub const SQL_C_UBIGINT: SqlSmallInt = SQL_BIGINT + SQL_UNSIGNED_OFFSET; // -27
pub const SQL_C_BIT: SqlSmallInt = SQL_BIT; // -7
pub const SQL_C_BINARY: SqlSmallInt = SQL_BINARY; // -2
pub const SQL_C_GUID: SqlSmallInt = SQL_GUID; // -11
pub const SQL_C_NUMERIC: SqlSmallInt = SQL_NUMERIC; // 2
pub const SQL_C_FLOAT: SqlSmallInt = SQL_REAL; // 7
pub const SQL_C_DOUBLE: SqlSmallInt = SQL_DOUBLE; // 8

// Legacy (ODBC 2.x) date/time C types plus the ODBC 3.x `SQL_C_TYPE_*` forms.
pub const SQL_C_DATE: SqlSmallInt = 9;
pub const SQL_C_TIME: SqlSmallInt = 10;
pub const SQL_C_TIMESTAMP: SqlSmallInt = 11;
pub const SQL_C_TYPE_DATE: SqlSmallInt = 91;
pub const SQL_C_TYPE_TIME: SqlSmallInt = 92;
pub const SQL_C_TYPE_TIMESTAMP: SqlSmallInt = 93;

// SQL Server-specific C types (msodbcsql extensions). `SQL_C_TYPES_EXTENDED`
// is `0x4000`; the two SS date/time C types are offset from it.
pub const SQL_C_TYPES_EXTENDED: SqlSmallInt = 0x4000; // 16384
pub const SQL_C_SS_TIME2: SqlSmallInt = SQL_C_TYPES_EXTENDED; // 0x4000
pub const SQL_C_SS_TIMESTAMPOFFSET: SqlSmallInt = SQL_C_TYPES_EXTENDED + 1; // 0x4001

// SQLColAttribute field identifier for the underlying type of a `sql_variant`
// column (msodbcsql: `SQL_CA_SS_BASE + 15`). Required by mssql-python's
// sql_variant probe.
pub const SQL_CA_SS_VARIANT_TYPE: SqlUSmallInt = 1215;

// ---- Statement attribute identifiers (SQLSetStmtAttr / SQLGetStmtAttr) ------
pub const SQL_ATTR_ROW_BIND_TYPE: SqlInteger = 5;
pub const SQL_ATTR_CURSOR_TYPE: SqlInteger = 6;
pub const SQL_ATTR_CONCURRENCY: SqlInteger = 7;
pub const SQL_ATTR_PARAM_BIND_TYPE: SqlInteger = 18;
pub const SQL_ATTR_PARAM_STATUS_PTR: SqlInteger = 20;
pub const SQL_ATTR_PARAMS_PROCESSED_PTR: SqlInteger = 21;
pub const SQL_ATTR_PARAMSET_SIZE: SqlInteger = 22;
pub const SQL_ATTR_ROW_BIND_OFFSET_PTR: SqlInteger = 23;
pub const SQL_ATTR_ROW_STATUS_PTR: SqlInteger = 25;
pub const SQL_ATTR_ROWS_FETCHED_PTR: SqlInteger = 26;
pub const SQL_ATTR_ROW_ARRAY_SIZE: SqlInteger = 27;
pub const SQL_ATTR_APP_ROW_DESC: SqlInteger = 10010;
pub const SQL_ATTR_APP_PARAM_DESC: SqlInteger = 10011;

/// `SQL_ATTR_ROW_BIND_TYPE` value selecting column-wise (array-of-columns)
/// binding — the mode mssql-python uses.
pub const SQL_BIND_BY_COLUMN: SqlULen = 0;
/// `SQL_ATTR_CURSOR_TYPE` value: forward-only (the only mode this driver
/// supports).
pub const SQL_CURSOR_FORWARD_ONLY: SqlULen = 0;
/// `SQL_ATTR_CONCURRENCY` value: read-only.
pub const SQL_CONCUR_READ_ONLY: SqlULen = 1;

// Per-row status codes written into a `SQL_ATTR_ROW_STATUS_PTR` array.
pub const SQL_ROW_SUCCESS: SqlUSmallInt = 0;
pub const SQL_ROW_SUCCESS_WITH_INFO: SqlUSmallInt = 6;
pub const SQL_ROW_NOROW: SqlUSmallInt = 3;

// ---- ODBC C interop structs (SQLBindCol / SQLGetData targets) ---------------
/// Maximum byte length of a `SQL_NUMERIC_STRUCT` mantissa.
pub const SQL_MAX_NUMERIC_LEN: usize = 16;

/// ODBC `SQL_DATE_STRUCT`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SqlDateStruct {
    pub year: SqlSmallInt,
    pub month: SqlUSmallInt,
    pub day: SqlUSmallInt,
}

/// ODBC `SQL_TIME_STRUCT`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SqlTimeStruct {
    pub hour: SqlUSmallInt,
    pub minute: SqlUSmallInt,
    pub second: SqlUSmallInt,
}

/// ODBC `SQL_TIMESTAMP_STRUCT`. `fraction` is in nanoseconds.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SqlTimestampStruct {
    pub year: SqlSmallInt,
    pub month: SqlUSmallInt,
    pub day: SqlUSmallInt,
    pub hour: SqlUSmallInt,
    pub minute: SqlUSmallInt,
    pub second: SqlUSmallInt,
    pub fraction: SqlUInteger,
}

/// msodbcsql `SQL_SS_TIME2_STRUCT`. `fraction` is in nanoseconds.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SqlSsTime2Struct {
    pub hour: SqlUSmallInt,
    pub minute: SqlUSmallInt,
    pub second: SqlUSmallInt,
    pub fraction: SqlUInteger,
}

/// msodbcsql `SQL_SS_TIMESTAMPOFFSET_STRUCT`. `fraction` is in nanoseconds and
/// the timezone offset is split into signed hour/minute components.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SqlSsTimestampoffsetStruct {
    pub year: SqlSmallInt,
    pub month: SqlUSmallInt,
    pub day: SqlUSmallInt,
    pub hour: SqlUSmallInt,
    pub minute: SqlUSmallInt,
    pub second: SqlUSmallInt,
    pub fraction: SqlUInteger,
    pub timezone_hour: SqlSmallInt,
    pub timezone_minute: SqlSmallInt,
}

/// ODBC `SQLGUID`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SqlGuid {
    pub data1: SqlUInteger,
    pub data2: SqlUSmallInt,
    pub data3: SqlUSmallInt,
    pub data4: [u8; 8],
}

/// ODBC `SQL_NUMERIC_STRUCT`. `val` holds a little-endian unsigned mantissa;
/// `sign` is 1 for positive, 0 for negative.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SqlNumericStruct {
    pub precision: u8,
    pub scale: i8,
    pub sign: u8,
    pub val: [u8; SQL_MAX_NUMERIC_LEN],
}
