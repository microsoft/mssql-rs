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
pub const SQL_ATTR_AUTOCOMMIT: SqlInteger = 102;
pub const SQL_ATTR_LOGIN_TIMEOUT: SqlInteger = 103;
pub const SQL_ATTR_TXN_ISOLATION: SqlInteger = 108;
pub const SQL_ATTR_PACKET_SIZE: SqlInteger = 112;
pub const SQL_ATTR_CONNECTION_TIMEOUT: SqlInteger = 113;
pub const SQL_ATTR_ANSI_APP: SqlInteger = 115;

// Connection-pooling attributes.
//
// `SQL_ATTR_RESET_CONNECTION` is armed by the pool at check-in so the next
// request resets the session to its login defaults; `SQL_ATTR_CONNECTION_DEAD`
// is a read-only liveness flag the pool consults before handing a connection
// out.
pub const SQL_ATTR_RESET_CONNECTION: SqlInteger = 116;
pub const SQL_ATTR_CONNECTION_DEAD: SqlInteger = 1209;
// Callers reaching us through the Windows Driver Manager must use the msodbcsql
// spelling: the DM reserves SQL_ATTR_RESET_CONNECTION for its own pooling
// protocol and rejects it from applications. Callers that load this driver
// directly (mssql-python) send SQL_ATTR_RESET_CONNECTION on every platform.
pub const SQL_COPT_SS_RESET_CONNECTION: SqlInteger = 1246;

// `SQL_ATTR_RESET_CONNECTION` value. Only `SQL_RESET_CONNECTION_YES` is defined;
// any other value is HY024.
pub const SQL_RESET_CONNECTION_YES: u32 = 1;

// `SQL_ATTR_CONNECTION_DEAD` values. msodbcsql reports DEAD until a token read
// succeeds, so disconnected/never-connected reads DEAD.
pub const SQL_CD_TRUE: u32 = 1;
pub const SQL_CD_FALSE: u32 = 0;

// `SQL_ATTR_ACCESS_MODE` values.
pub const SQL_MODE_READ_WRITE: u32 = 0;

// `SQL_ATTR_AUTOCOMMIT` values. The ODBC-mandated default is `SQL_AUTOCOMMIT_ON`
// (msodbcsql `sqlcfunc.cpp:3462`); manual-commit is opt-in per connection.
pub const SQL_AUTOCOMMIT_OFF: u32 = 0;
pub const SQL_AUTOCOMMIT_ON: u32 = 1;

// `SQL_ATTR_TXN_ISOLATION` values. These are ODBC *bitmask* bits, distinct from
// the T-SQL `SET TRANSACTION ISOLATION LEVEL` text they map to — see
// `txn::txn_isolation_to_tsql`.
pub const SQL_TXN_READ_UNCOMMITTED: u32 = 0x0000_0001;
pub const SQL_TXN_READ_COMMITTED: u32 = 0x0000_0002;
pub const SQL_TXN_REPEATABLE_READ: u32 = 0x0000_0004;
pub const SQL_TXN_SERIALIZABLE: u32 = 0x0000_0008;
/// SQL Server extension (`msodbcsql.h:465`).
pub const SQL_TXN_SS_SNAPSHOT: u32 = 0x0000_0020;

/// SQL Server-specific connection attribute for isolation level
/// (`msodbcsql.h:200`).
///
/// The Windows Driver Manager screens `SQL_ATTR_TXN_ISOLATION` against the four
/// standard ODBC bits and rejects anything else with HY024 before the driver is
/// ever called, so this vendor attribute is the only route by which an
/// application can select `SQL_TXN_SS_SNAPSHOT`.
pub const SQL_COPT_SS_TXN_ISOLATION: SqlInteger = 1227;

// `SQLEndTran` completion types.
pub const SQL_COMMIT: SqlSmallInt = 0;
pub const SQL_ROLLBACK: SqlSmallInt = 1;

/// Default `SQL_ATTR_PACKET_SIZE`, matching `ClientContext::packet_size`.
pub const DEFAULT_PACKET_SIZE: u32 = 8000;

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
pub const SQL_API_SQLCOLATTRIBUTE: SqlUSmallInt = 6;
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
pub const SQL_API_SQLGETTYPEINFO: SqlUSmallInt = 47;
pub const SQL_API_SQLDESCRIBEPARAM: SqlUSmallInt = 58;
pub const SQL_API_SQLBINDPARAMETER: SqlUSmallInt = 72;
pub const SQL_API_SQLMORERESULTS: SqlUSmallInt = 61;
pub const SQL_API_SQLALLOCHANDLE: SqlUSmallInt = 1001;
pub const SQL_API_SQLCLOSECURSOR: SqlUSmallInt = 1003;
pub const SQL_API_SQLENDTRAN: SqlUSmallInt = 1005;
pub const SQL_API_SQLFREEHANDLE: SqlUSmallInt = 1006;
pub const SQL_API_SQLGETCONNECTATTR: SqlUSmallInt = 1007;
pub const SQL_API_SQLGETDIAGFIELD: SqlUSmallInt = 1010;
pub const SQL_API_SQLGETDIAGREC: SqlUSmallInt = 1011;
pub const SQL_API_SQLGETENVATTR: SqlUSmallInt = 1012;
pub const SQL_API_SQLGETSTMTATTR: SqlUSmallInt = 1014;
pub const SQL_API_SQLSETCONNECTATTR: SqlUSmallInt = 1016;
pub const SQL_API_SQLSETENVATTR: SqlUSmallInt = 1019;
pub const SQL_API_SQLSETSTMTATTR: SqlUSmallInt = 1020;

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
pub const SQL_DEFAULT_TXN_ISOLATION: SqlUSmallInt = 26;
pub const SQL_IDENTIFIER_QUOTE_CHAR: SqlUSmallInt = 29;
pub const SQL_MULTIPLE_ACTIVE_TXN: SqlUSmallInt = 37;
pub const SQL_TXN_CAPABLE: SqlUSmallInt = 46;
pub const SQL_TXN_ISOLATION_OPTION: SqlUSmallInt = 72;
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
/// `SQL_TXN_CAPABLE`: DML and DDL are both transactable (msodbcsql `sqlcinfo.cpp:323`).
pub const SQL_TC_ALL: u16 = 2;
/// `SQL_TXN_ISOLATION_OPTION` bitmask — the five levels this driver accepts
/// (msodbcsql `SQL_TXN_ISOLATION_OPTION_SPT`, `sqlcinfo.cpp:181-183`).
pub const SQL_TXN_ISOLATION_OPTION_SPT: u32 = SQL_TXN_READ_UNCOMMITTED
    | SQL_TXN_READ_COMMITTED
    | SQL_TXN_REPEATABLE_READ
    | SQL_TXN_SERIALIZABLE
    | SQL_TXN_SS_SNAPSHOT;
pub const SQL_GD_ANY_COLUMN: u32 = 0x00000001;
pub const SQL_GD_ANY_ORDER: u32 = 0x00000002;
pub const SQL_ASYNC_DBC_NOT_CAPABLE: u32 = 0x00000000;
pub const SQL_ASYNC_NOTIFICATION_NOT_CAPABLE: u32 = 0x00000000;

// ODBC-SQL-type identifiers.
pub const SQL_UNKNOWN_TYPE: SqlSmallInt = 0;
/// `SQLGetTypeInfo` sentinel requesting information for all data types.
pub const SQL_ALL_TYPES: SqlSmallInt = 0;
pub const SQL_CHAR: SqlSmallInt = 1;
pub const SQL_NUMERIC: SqlSmallInt = 2;
pub const SQL_DECIMAL: SqlSmallInt = 3;
pub const SQL_INTEGER: SqlSmallInt = 4;
pub const SQL_SMALLINT: SqlSmallInt = 5;
pub const SQL_FLOAT: SqlSmallInt = 6;
pub const SQL_REAL: SqlSmallInt = 7;
pub const SQL_DOUBLE: SqlSmallInt = 8;
pub const SQL_DATETIME: SqlSmallInt = 9;
/// ODBC 2.x `SQL_TIME` concise type id (shares its value with no other id here).
pub const SQL_TIME: SqlSmallInt = 10;
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

/// Unbounded-length sentinel an application may pass as `ColumnSize` when
/// binding a `*(max)`, `xml` or UDT parameter; [`crate::conversion`] widens it
/// (and any oversized length) to `max`.
///
/// This is an *accepted input* only. `SQLDescribeParam` deliberately reports `0`
/// for unbounded parameters, which is what msodbcsql 18.6.2.1 does; the
/// `SQL_PREC_UNLIMITED` pass-through in `sqlcdesc.cpp` (`GetIPDRec`) is gated
/// off for the describe path. Named here so that a reader who finds the
/// sentinel in the msodbcsql source does not conclude the mapping is missing.
pub const SQL_PREC_UNLIMITED: SqlULen = 2_147_483_647;

/// Header of the `SQL_SS_VECTOR` client buffer that precedes the element array.
///
/// Only its size participates in the `ParameterSize` <-> dimension conversion,
/// so the fields are never read directly.
#[repr(C)]
pub(crate) struct SqlSsVectorLayout {
    pub(crate) dimensions: u32,
    pub(crate) base_type: u8,
    pub(crate) reserved: [u8; 3],
}

/// Client-side element width of a `SQL_SS_VECTOR` buffer.
///
/// msodbcsql always materialises vector elements as 4-byte floats regardless of
/// the server-side base type, so `float16` vectors are widened on the way out
/// and narrowed on the way in.
pub(crate) const SQL_SS_VECTOR_ELEMENT_SIZE: usize = std::mem::size_of::<f32>();

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

// ---- SQL Server-specific ODBC-SQL-type identifiers (driver extensions) ------
// The `SS_TIME2`/`SS_TIMESTAMPOFFSET` ids are declared above; these complete the
// set used by the fetch path.
pub const SQL_SS_VARIANT: SqlSmallInt = -150;
pub const SQL_SS_UDT: SqlSmallInt = -151;
pub const SQL_SS_XML: SqlSmallInt = -152;
pub const SQL_SS_TABLE: SqlSmallInt = -153;
pub const SQL_SS_VECTOR: SqlSmallInt = -156;

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

// SQL Server-specific C types (msodbcsql extensions). Values confirmed against
// msodbcsql's `msodbcsql.h`: `#define SQL_C_TYPES_EXTENDED 0x04000L`.
pub const SQL_C_TYPES_EXTENDED: SqlSmallInt = 0x4000; // 16384
pub const SQL_C_SS_TIME2: SqlSmallInt = SQL_C_TYPES_EXTENDED; // 0x4000
pub const SQL_C_SS_TIMESTAMPOFFSET: SqlSmallInt = SQL_C_TYPES_EXTENDED + 1; // 0x4001
pub const SQL_C_SS_VECTOR: SqlSmallInt = SQL_C_TYPES_EXTENDED + 2; // 0x4002

// The 13 ODBC 3.x interval C types occupy a contiguous range, 101..=113, and the
// matching `SQL_INTERVAL_*` SQL types share those values.
pub const SQL_C_INTERVAL_YEAR: SqlSmallInt = 101;
pub const SQL_C_INTERVAL_MINUTE_TO_SECOND: SqlSmallInt = 113;
// The interval SQL types share the C type values above.
pub const SQL_INTERVAL_YEAR: SqlSmallInt = 101;
pub const SQL_INTERVAL_MINUTE_TO_SECOND: SqlSmallInt = 113;

// SQLColAttribute field identifier for the underlying type of a `sql_variant`
// column (msodbcsql: `SQL_CA_SS_BASE + 15`). Required by mssql-python's
// sql_variant probe.
pub const SQL_CA_SS_VARIANT_TYPE: SqlUSmallInt = 1215;

// ---- SQLColAttribute field identifiers --------------------------------------
// The low-numbered ids are the ODBC 3.x descriptor fields that double as the
// ODBC 2.x `SQL_COLUMN_*` ids; the 100x block is descriptor-header/record only.
pub const SQL_DESC_CONCISE_TYPE: SqlUSmallInt = 2;
pub const SQL_DESC_DISPLAY_SIZE: SqlUSmallInt = 6;
pub const SQL_DESC_UNSIGNED: SqlUSmallInt = 8;
pub const SQL_DESC_FIXED_PREC_SCALE: SqlUSmallInt = 9;
pub const SQL_DESC_UPDATABLE: SqlUSmallInt = 10;
pub const SQL_DESC_AUTO_UNIQUE_VALUE: SqlUSmallInt = 11;
pub const SQL_DESC_CASE_SENSITIVE: SqlUSmallInt = 12;
pub const SQL_DESC_SEARCHABLE: SqlUSmallInt = 13;
pub const SQL_DESC_TYPE_NAME: SqlUSmallInt = 14;
pub const SQL_DESC_TABLE_NAME: SqlUSmallInt = 15;
pub const SQL_DESC_SCHEMA_NAME: SqlUSmallInt = 16;
pub const SQL_DESC_CATALOG_NAME: SqlUSmallInt = 17;
pub const SQL_DESC_LABEL: SqlUSmallInt = 18;
pub const SQL_DESC_BASE_COLUMN_NAME: SqlUSmallInt = 22;
pub const SQL_DESC_BASE_TABLE_NAME: SqlUSmallInt = 23;
pub const SQL_DESC_NUM_PREC_RADIX: SqlUSmallInt = 32;
pub const SQL_DESC_COUNT: SqlUSmallInt = 1001;
pub const SQL_DESC_TYPE: SqlUSmallInt = 1002;
pub const SQL_DESC_LENGTH: SqlUSmallInt = 1003;
pub const SQL_DESC_PRECISION: SqlUSmallInt = 1005;
pub const SQL_DESC_SCALE: SqlUSmallInt = 1006;
pub const SQL_DESC_NULLABLE: SqlUSmallInt = 1008;
pub const SQL_DESC_DATETIME_INTERVAL_CODE: SqlUSmallInt = 1007;
pub const SQL_DESC_NAME: SqlUSmallInt = 1011;
pub const SQL_DESC_UNNAMED: SqlUSmallInt = 1012;
pub const SQL_DESC_OCTET_LENGTH: SqlUSmallInt = 1013;

// ---- SQLColAttribute value constants ----------------------------------------
pub const SQL_NULLABLE_UNKNOWN: SqlLen = 2;
pub const SQL_NAMED: SqlLen = 0;
pub const SQL_UNNAMED: SqlLen = 1;
// SQL_DESC_DATETIME_INTERVAL_CODE, valid when the verbose type is SQL_DATETIME.
pub const SQL_CODE_DATE: SqlLen = 1;
pub const SQL_CODE_TIME: SqlLen = 2;
pub const SQL_CODE_TIMESTAMP: SqlLen = 3;
// SQL_DESC_SEARCHABLE
pub const SQL_PRED_NONE: SqlLen = 0;
pub const SQL_PRED_CHAR: SqlLen = 1;
pub const SQL_PRED_BASIC: SqlLen = 2;
pub const SQL_PRED_SEARCHABLE: SqlLen = 3;
// SQL_DESC_UPDATABLE
pub const SQL_ATTR_READONLY: SqlLen = 0;
pub const SQL_ATTR_READWRITE_UNKNOWN: SqlLen = 2;

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
