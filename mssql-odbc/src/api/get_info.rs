// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLGetInfoW.

use tracing::{debug, error};

use super::current_catalog::resolved_current_catalog;
use crate::api::odbc_types as odbc;
use crate::api::odbc_types::{
    SQL_ACCESSIBLE_PROCEDURES, SQL_ACCESSIBLE_TABLES, SQL_ACTIVE_STATEMENTS,
    SQL_ASYNC_DBC_FUNCTIONS, SQL_ASYNC_DBC_NOT_CAPABLE, SQL_ASYNC_NOTIFICATION,
    SQL_ASYNC_NOTIFICATION_NOT_CAPABLE, SQL_CATALOG_NAME_SEPARATOR, SQL_CATALOG_TERM, SQL_CB_CLOSE,
    SQL_CONVERT_FUNCTIONS, SQL_CONVERT_FUNCTIONS_SUPPORTED, SQL_CURSOR_COMMIT_BEHAVIOR,
    SQL_CURSOR_ROLLBACK_BEHAVIOR, SQL_DATA_SOURCE_NAME, SQL_DATA_SOURCE_READ_ONLY,
    SQL_DATABASE_NAME, SQL_DBMS_NAME, SQL_DBMS_VER, SQL_DEFAULT_TXN_ISOLATION, SQL_DM_VER,
    SQL_DRIVER_NAME, SQL_DRIVER_ODBC_VER, SQL_DRIVER_VER, SQL_ERROR, SQL_EXPRESSIONS_IN_ORDERBY,
    SQL_GD_ANY_COLUMN, SQL_GD_ANY_ORDER, SQL_GETDATA_EXTENSIONS, SQL_IDENTIFIER_QUOTE_CHAR,
    SQL_INVALID_HANDLE, SQL_KEYWORDS, SQL_LIKE_ESCAPE_CLAUSE, SQL_MAX_COLUMN_NAME_LEN,
    SQL_MAX_DRIVER_CONNECTIONS, SQL_MAX_SCHEMA_NAME_LEN, SQL_MAX_STATEMENT_LEN,
    SQL_MAX_TABLE_NAME_LEN, SQL_MULTIPLE_ACTIVE_TXN, SQL_NEED_LONG_DATA_LEN, SQL_NUMERIC_FUNCTIONS,
    SQL_NUMERIC_FUNCTIONS_SUPPORTED, SQL_OAC_LEVEL2, SQL_ODBC_API_CONFORMANCE,
    SQL_ODBC_SQL_CONFORMANCE, SQL_ODBC_VER, SQL_OJ_CAPABILITIES, SQL_OJ_CAPABILITIES_SUPPORTED,
    SQL_OSC_CORE, SQL_OUTER_JOINS, SQL_PARAM_ARRAY_ROW_COUNTS, SQL_PARAM_ARRAY_SELECTS,
    SQL_PARC_NO_BATCH, SQL_PAS_BATCH, SQL_PROCEDURES, SQL_SC_SQL92_ENTRY, SQL_SCHEMA_TERM,
    SQL_SERVER_NAME, SQL_SPECIAL_CHARACTERS, SQL_SQL_CONFORMANCE, SQL_STRING_FUNCTIONS,
    SQL_STRING_FUNCTIONS_SUPPORTED, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SQL_SYSTEM_FUNCTIONS,
    SQL_SYSTEM_FUNCTIONS_SUPPORTED, SQL_TC_ALL, SQL_TIMEDATE_ADD_INTERVALS,
    SQL_TIMEDATE_DIFF_INTERVALS, SQL_TIMEDATE_FUNCTIONS, SQL_TIMEDATE_FUNCTIONS_SUPPORTED,
    SQL_TIMEDATE_INTERVALS_SUPPORTED, SQL_TXN_CAPABLE, SQL_TXN_ISOLATION_OPTION,
    SQL_TXN_ISOLATION_OPTION_SPT, SQL_TXN_READ_COMMITTED, SQL_USER_NAME, SqlHandle, SqlPointer,
    SqlReturn, SqlSmallInt, SqlUSmallInt, SqlWChar,
};
use crate::api::sqlstate::{
    ERR_INVALID_INFO_TYPE, ERR_INVALID_STRING_OR_BUFFER_LENGTH, WARN_STRING_TRUNCATION, post_diag,
};
use crate::api::txn::try_claim_idle_dbc_client;
use crate::api::util::{copy_with_nul, write_if_some};
use crate::error::free_errors;
use crate::handles::dbc::{CachedDatabaseUserName, ConnectionState};
use crate::handles::{DbcHandle, HandleType, handle_from_raw};
use mssql_tds::connection::tds_client::{ExecuteOptions, ResultSet, TdsClient};
use mssql_tds::datatypes::column_values::ColumnValues;

/// `sysname`, the type of every identifier column in the catalog views, which
/// bounds `SQL_MAX_COLUMN_NAME_LEN`, `SQL_MAX_SCHEMA_NAME_LEN`, and
/// `SQL_MAX_TABLE_NAME_LEN` alike.
const MAX_IDENTIFIER_LEN: u16 = 128;

/// The `SQL_USER_NAME` lookup. See [`fetch_database_user_name`] for why this is
/// the whole of msodbcsql's `g_szSqlUdtQuery` that applies to this driver.
const DATABASE_USER_NAME_QUERY: &str = "SELECT USER_NAME()";

const MAX_SQL_BLOCKS: u32 = 128;
const NO_CAPABILITIES: u32 = 0;
const NO_STATED_U16_LIMIT: u16 = 0;
// Matches msodbcsql's SQL_ALTER_TABLE_SPT (sqlcinfo.cpp), decomposed from the
// SQL_AT_* bitmasks in sqlext.h: ADD_COLUMN | ADD_CONSTRAINT |
// ADD_COLUMN_SINGLE | ADD_COLUMN_DEFAULT | ADD_TABLE_CONSTRAINT |
// CONSTRAINT_NAME_DEFINITION | DROP_COLUMN_RESTRICT.
const SQL_AT_ADD_COLUMN: u32 = 0x0000_0001;
const SQL_AT_ADD_CONSTRAINT: u32 = 0x0000_0008;
const SQL_AT_ADD_COLUMN_SINGLE: u32 = 0x0000_0020;
const SQL_AT_ADD_COLUMN_DEFAULT: u32 = 0x0000_0040;
const SQL_AT_DROP_COLUMN_RESTRICT: u32 = 0x0000_0800;
const SQL_AT_ADD_TABLE_CONSTRAINT: u32 = 0x0000_1000;
const SQL_AT_CONSTRAINT_NAME_DEFINITION: u32 = 0x0000_8000;
const MSODBCSQL_ALTER_TABLE_CAPABILITIES: u32 = SQL_AT_ADD_COLUMN
    | SQL_AT_ADD_CONSTRAINT
    | SQL_AT_ADD_COLUMN_SINGLE
    | SQL_AT_ADD_COLUMN_DEFAULT
    | SQL_AT_DROP_COLUMN_RESTRICT
    | SQL_AT_ADD_TABLE_CONSTRAINT
    | SQL_AT_CONSTRAINT_NAME_DEFINITION;
const SQL_SERVER_MAX_INDEX_COLUMNS: u16 = 16;
const SQL_SERVER_MAX_SELECT_COLUMNS: u16 = 4096;
const SQL_SERVER_MAX_TABLE_COLUMNS: u16 = 1024;
const SQL_SERVER_MAX_ROW_SIZE: u32 = 8060;
const SQL_SERVER_MAX_TABLES_IN_SELECT: u16 = 32;

// AB#47996: SQLGetInfo conversion bitmask components (sqlext.h SQL_CVT_*).
const CVT_CHAR: u32 = 0x0000_0001;
const CVT_NUMERIC: u32 = 0x0000_0002;
const CVT_DECIMAL: u32 = 0x0000_0004;
const CVT_INTEGER: u32 = 0x0000_0008;
const CVT_SMALLINT: u32 = 0x0000_0010;
const CVT_FLOAT: u32 = 0x0000_0020;
const CVT_REAL: u32 = 0x0000_0040;
const CVT_VARCHAR: u32 = 0x0000_0100;
const CVT_LONGVARCHAR: u32 = 0x0000_0200;
const CVT_BINARY: u32 = 0x0000_0400;
const CVT_VARBINARY: u32 = 0x0000_0800;
const CVT_BIT: u32 = 0x0000_1000;
const CVT_TINYINT: u32 = 0x0000_2000;
const CVT_BIGINT: u32 = 0x0000_4000;
const CVT_TIMESTAMP: u32 = 0x0002_0000;
const CVT_LONGVARBINARY: u32 = 0x0004_0000;
const CVT_WCHAR: u32 = 0x0020_0000;
const CVT_WLONGVARCHAR: u32 = 0x0040_0000;
const CVT_WVARCHAR: u32 = 0x0080_0000;
const CVT_GUID: u32 = 0x0100_0000;

// msodbcsql conversion groupings (`sqlcinfo.cpp` macros). `MONEYCVT` is
// `SQL_CVT_DECIMAL`, `IMAGECVT` is `SQL_CVT_LONGVARBINARY`, `TIMESTAMPCVT` is
// `SQL_CVT_TIMESTAMP`, `GUIDCVT` is `SQL_CVT_GUID`.
const BINARYCVT: u32 = CVT_BINARY | CVT_VARBINARY;
const INTCVT: u32 = CVT_BIGINT | CVT_INTEGER | CVT_SMALLINT | CVT_TINYINT;
const FLOATCVT: u32 = CVT_FLOAT | CVT_REAL;
const WCHARCVT: u32 = CVT_WCHAR | CVT_WVARCHAR;
const CHARCVT: u32 = CVT_CHAR | CVT_VARCHAR;
const NUMERICCVT: u32 = CVT_DECIMAL | CVT_NUMERIC;
const TEXTCVT: u32 = CVT_LONGVARCHAR | CVT_WLONGVARCHAR;
const CVT_CHAR_SPT: u32 = BINARYCVT
    | INTCVT
    | FLOATCVT
    | CHARCVT
    | WCHARCVT
    | CVT_DECIMAL
    | CVT_BIT
    | NUMERICCVT
    | CVT_TIMESTAMP
    | TEXTCVT
    | CVT_LONGVARBINARY
    | CVT_GUID;
const CVT_BINARY_SPT: u32 =
    BINARYCVT | INTCVT | CHARCVT | WCHARCVT | CVT_LONGVARBINARY | NUMERICCVT;
const CVT_NUMBER_SPT: u32 =
    BINARYCVT | INTCVT | FLOATCVT | CHARCVT | WCHARCVT | CVT_DECIMAL | CVT_BIT | NUMERICCVT;
const CVT_APXNUM_SPT: u32 =
    INTCVT | FLOATCVT | CHARCVT | WCHARCVT | CVT_DECIMAL | CVT_BIT | NUMERICCVT;
const CVT_BIT_SPT: u32 = BINARYCVT | INTCVT | FLOATCVT | CHARCVT | WCHARCVT | CVT_BIT | NUMERICCVT;
const CVT_VARCHAR_SPT: u32 = CVT_CHAR_SPT;
const CVT_GUID_SPT: u32 = CHARCVT | WCHARCVT | CVT_GUID;
const CVT_LONGVARCHAR_SPT: u32 = CHARCVT | WCHARCVT | TEXTCVT;
const CVT_LONGVARBINARY_SPT: u32 = BINARYCVT | CVT_LONGVARBINARY;
const CVT_TIMESTAMP_SPT: u32 = BINARYCVT | CHARCVT | WCHARCVT | CVT_TIMESTAMP;

// Other AB#47996 capability masks (sqlext.h bit names).
const SQL_AF_ALL: u32 = 0x0000_0040;
const SQL_CT_CREATE_TABLE: u32 = 0x0000_0001;
const SQL_DT_DROP_TABLE: u32 = 0x0000_0001;
const SQL_DV_DROP_VIEW: u32 = 0x0000_0001;
const SQL_CS_CREATE_SCHEMA: u32 = 0x0000_0001;
const SQL_CS_AUTHORIZATION: u32 = 0x0000_0002;
const SQL_IK_ALL: u32 = 0x0000_0001 | 0x0000_0002;
const SQL_IS_INSERT_LITERALS: u32 = 0x0000_0001;
const SQL_IS_INSERT_SEARCHED: u32 = 0x0000_0002;
const SQL_IS_SELECT_INTO: u32 = 0x0000_0004;
const SQL_SCC_ISO92_CLI: u32 = 0x0000_0002;
const SQL_QL_START: u16 = 0x0001;
const SQL_NNC_NON_NULL: u16 = 0x0001;
const SQL_FILE_NOT_SUPPORTED: u16 = 0x0000;
// The `INFORMATION_SCHEMA` views SQL Server exposes (msodbcsql
// `SQL_INFO_SCHEMA_VIEWS_SPT`): the 17 `SQL_ISV_*` bits it advertises.
const SQL_INFO_SCHEMA_VIEWS_MASK: u32 = 0x0000_0004 // CHECK_CONSTRAINTS
    | 0x0000_0010 // COLUMN_DOMAIN_USAGE
    | 0x0000_0020 // COLUMN_PRIVILEGES
    | 0x0000_0040 // COLUMNS
    | 0x0000_0080 // CONSTRAINT_COLUMN_USAGE
    | 0x0000_0100 // CONSTRAINT_TABLE_USAGE
    | 0x0000_0200 // DOMAIN_CONSTRAINTS
    | 0x0000_0400 // DOMAINS
    | 0x0000_0800 // KEY_COLUMN_USAGE
    | 0x0000_1000 // REFERENTIAL_CONSTRAINTS
    | 0x0000_2000 // SCHEMATA
    | 0x0000_8000 // TABLE_CONSTRAINTS
    | 0x0001_0000 // TABLE_PRIVILEGES
    | 0x0002_0000 // TABLES
    | 0x0010_0000 // VIEW_COLUMN_USAGE
    | 0x0020_0000 // VIEW_TABLE_USAGE
    | 0x0040_0000; // VIEWS
// SQL Server identifier and index limits from msodbcsql's `sqlsrv.h`/`tds.h`:
// `MAXCURSORNAMESPHINX` = `SYSNAMELEN` (128), `MAXPROCNAMESPHINX` =
// `MAX_PROCNAME + 6` = 128 + 6 (the `;nnnnn` numbered-procedure suffix), and
// `MAXINDEXSIZESPHINX` = 900. The compare-leg E2E pins these against retail.
const SQL_SERVER_MAX_CURSOR_NAME_LEN: u16 = 128;
const SQL_SERVER_MAX_PROCEDURE_NAME_LEN: u16 = 134;
const SQL_SERVER_MAX_INDEX_SIZE: u32 = 900;

// `SQL_CREATE_VIEW` and the SQL-92 capability masks (sqlext.h bit names),
// assembled to match msodbcsql's `SQLGetInfoTable`.
const SQL_CV_CREATE_VIEW: u32 = 0x0000_0001;
const SQL_CV_CHECK_OPTION: u32 = 0x0000_0002;
const SQL_CREATE_VIEW_MASK: u32 = SQL_CV_CREATE_VIEW | SQL_CV_CHECK_OPTION;
const SQL_SG_WITH_GRANT_OPTION: u32 = 0x0000_0010;
const SQL_SR_GRANT_OPTION_FOR: u32 = 0x0000_0010;
const SQL_SP_EXISTS: u32 = 0x0000_0001;
const SQL_SP_ISNOTNULL: u32 = 0x0000_0002;
const SQL_SP_ISNULL: u32 = 0x0000_0004;
const SQL_SP_LIKE: u32 = 0x0000_0200;
const SQL_SP_IN: u32 = 0x0000_0400;
const SQL_SP_BETWEEN: u32 = 0x0000_0800;
const SQL_SP_COMPARISON: u32 = 0x0000_1000;
const SQL_SP_QUANTIFIED_COMPARISON: u32 = 0x0000_2000;
const SQL_SQL92_PREDICATES_SPT: u32 = SQL_SP_BETWEEN
    | SQL_SP_COMPARISON
    | SQL_SP_EXISTS
    | SQL_SP_IN
    | SQL_SP_ISNOTNULL
    | SQL_SP_ISNULL
    | SQL_SP_LIKE
    | SQL_SP_QUANTIFIED_COMPARISON;
const SQL_SRJO_CROSS_JOIN: u32 = 0x0000_0002;
const SQL_SRJO_FULL_OUTER_JOIN: u32 = 0x0000_0008;
const SQL_SRJO_INNER_JOIN: u32 = 0x0000_0010;
const SQL_SRJO_LEFT_OUTER_JOIN: u32 = 0x0000_0040;
const SQL_SRJO_RIGHT_OUTER_JOIN: u32 = 0x0000_0100;
const SQL_SRJO_UNION_JOIN: u32 = 0x0000_0200;
const SQL_SQL92_RELATIONAL_JOIN_OPERATORS_SPT: u32 = SQL_SRJO_CROSS_JOIN
    | SQL_SRJO_FULL_OUTER_JOIN
    | SQL_SRJO_INNER_JOIN
    | SQL_SRJO_LEFT_OUTER_JOIN
    | SQL_SRJO_RIGHT_OUTER_JOIN
    | SQL_SRJO_UNION_JOIN;
const SQL_SRVC_VALUE_EXPRESSION: u32 = 0x0000_0001;
const SQL_SRVC_NULL: u32 = 0x0000_0002;
const SQL_SRVC_DEFAULT: u32 = 0x0000_0004;
const SQL_SRVC_ROW_SUBQUERY: u32 = 0x0000_0008;
const SQL_SQL92_ROW_VALUE_CONSTRUCTOR_SPT: u32 =
    SQL_SRVC_VALUE_EXPRESSION | SQL_SRVC_NULL | SQL_SRVC_DEFAULT | SQL_SRVC_ROW_SUBQUERY;
const SQL_SSF_LOWER: u32 = 0x0000_0002;
const SQL_SSF_UPPER: u32 = 0x0000_0004;
const SQL_SQL92_STRING_FUNCTIONS_SPT: u32 = SQL_SSF_LOWER | SQL_SSF_UPPER;
const SQL_SVE_CASE: u32 = 0x0000_0001;
const SQL_SVE_CAST: u32 = 0x0000_0002;
const SQL_SVE_COALESCE: u32 = 0x0000_0004;
const SQL_SVE_NULLIF: u32 = 0x0000_0008;
const SQL_SQL92_VALUE_EXPRESSIONS_SPT: u32 =
    SQL_SVE_CASE | SQL_SVE_CAST | SQL_SVE_COALESCE | SQL_SVE_NULLIF;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InfoValue {
    String(&'static str),
    U16(u16),
    U32(u32),
    Bitmask(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct InfoEntry {
    info_type: SqlUSmallInt,
    value: InfoValue,
}

const STATIC_INFO: &[InfoEntry] = &[
    InfoEntry {
        info_type: odbc::SQL_ASYNC_MODE,
        value: InfoValue::U32(odbc::SQL_AM_NONE),
    },
    InfoEntry {
        info_type: odbc::SQL_BATCH_ROW_COUNT,
        value: InfoValue::Bitmask(odbc::SQL_BRC_EXPLICIT),
    },
    InfoEntry {
        info_type: odbc::SQL_BATCH_SUPPORT,
        value: InfoValue::Bitmask(
            odbc::SQL_BS_SELECT_EXPLICIT
                | odbc::SQL_BS_ROW_COUNT_EXPLICIT
                | odbc::SQL_BS_SELECT_PROC
                | odbc::SQL_BS_ROW_COUNT_PROC,
        ),
    },
    InfoEntry {
        info_type: odbc::SQL_DYNAMIC_CURSOR_ATTRIBUTES1,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    InfoEntry {
        info_type: odbc::SQL_DYNAMIC_CURSOR_ATTRIBUTES2,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    InfoEntry {
        info_type: odbc::SQL_FORWARD_ONLY_CURSOR_ATTRIBUTES1,
        value: InfoValue::Bitmask(odbc::SQL_CA1_NEXT),
    },
    InfoEntry {
        info_type: odbc::SQL_FORWARD_ONLY_CURSOR_ATTRIBUTES2,
        value: InfoValue::Bitmask(
            odbc::SQL_CA2_READ_ONLY_CONCURRENCY | odbc::SQL_CA2_MAX_ROWS_SELECT,
        ),
    },
    InfoEntry {
        info_type: odbc::SQL_KEYSET_CURSOR_ATTRIBUTES1,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    InfoEntry {
        info_type: odbc::SQL_KEYSET_CURSOR_ATTRIBUTES2,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    InfoEntry {
        info_type: odbc::SQL_SEARCH_PATTERN_ESCAPE,
        value: InfoValue::String("\\"),
    },
    InfoEntry {
        info_type: odbc::SQL_STATIC_CURSOR_ATTRIBUTES1,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    InfoEntry {
        info_type: odbc::SQL_STATIC_CURSOR_ATTRIBUTES2,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    InfoEntry {
        info_type: odbc::SQL_BOOKMARK_PERSISTENCE,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    InfoEntry {
        info_type: odbc::SQL_CONCAT_NULL_BEHAVIOR,
        value: InfoValue::U16(odbc::SQL_CB_NULL),
    },
    InfoEntry {
        info_type: odbc::SQL_CURSOR_SENSITIVITY,
        value: InfoValue::U32(odbc::SQL_UNSPECIFIED),
    },
    InfoEntry {
        info_type: odbc::SQL_DESCRIBE_PARAMETER,
        value: InfoValue::String("Y"),
    },
    InfoEntry {
        info_type: odbc::SQL_MULT_RESULT_SETS,
        value: InfoValue::String("Y"),
    },
    InfoEntry {
        info_type: odbc::SQL_NULL_COLLATION,
        value: InfoValue::U16(odbc::SQL_NC_LOW),
    },
    InfoEntry {
        info_type: odbc::SQL_PROCEDURE_TERM,
        value: InfoValue::String("stored procedure"),
    },
    InfoEntry {
        info_type: odbc::SQL_SCROLL_OPTIONS,
        value: InfoValue::Bitmask(odbc::SQL_SO_FORWARD_ONLY),
    },
    InfoEntry {
        info_type: odbc::SQL_TABLE_TERM,
        value: InfoValue::String("table"),
    },
    InfoEntry {
        info_type: odbc::SQL_ALTER_TABLE,
        value: InfoValue::Bitmask(MSODBCSQL_ALTER_TABLE_CAPABILITIES),
    },
    InfoEntry {
        info_type: odbc::SQL_CATALOG_NAME,
        value: InfoValue::String("Y"),
    },
    InfoEntry {
        info_type: odbc::SQL_CATALOG_USAGE,
        value: InfoValue::Bitmask(
            odbc::SQL_CU_DML_STATEMENTS
                | odbc::SQL_CU_PROCEDURE_INVOCATION
                | odbc::SQL_CU_TABLE_DEFINITION,
        ),
    },
    InfoEntry {
        info_type: odbc::SQL_COLUMN_ALIAS,
        value: InfoValue::String("Y"),
    },
    InfoEntry {
        info_type: odbc::SQL_CORRELATION_NAME,
        value: InfoValue::U16(odbc::SQL_CN_ANY),
    },
    InfoEntry {
        info_type: odbc::SQL_CREATE_ASSERTION,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    InfoEntry {
        info_type: odbc::SQL_DDL_INDEX,
        value: InfoValue::Bitmask(odbc::SQL_DI_CREATE_INDEX | odbc::SQL_DI_DROP_INDEX),
    },
    InfoEntry {
        info_type: odbc::SQL_GROUP_BY,
        value: InfoValue::U16(odbc::SQL_GB_GROUP_BY_CONTAINS_SELECT),
    },
    InfoEntry {
        info_type: odbc::SQL_IDENTIFIER_CASE,
        value: InfoValue::U16(odbc::SQL_IC_MIXED),
    },
    InfoEntry {
        info_type: odbc::SQL_ORDER_BY_COLUMNS_IN_SELECT,
        value: InfoValue::String("N"),
    },
    InfoEntry {
        info_type: odbc::SQL_QUOTED_IDENTIFIER_CASE,
        value: InfoValue::U16(odbc::SQL_IC_MIXED),
    },
    InfoEntry {
        info_type: odbc::SQL_SCHEMA_USAGE,
        value: InfoValue::Bitmask(
            odbc::SQL_SU_DML_STATEMENTS
                | odbc::SQL_SU_PROCEDURE_INVOCATION
                | odbc::SQL_SU_TABLE_DEFINITION
                | odbc::SQL_SU_INDEX_DEFINITION
                | odbc::SQL_SU_PRIVILEGE_DEFINITION,
        ),
    },
    InfoEntry {
        info_type: odbc::SQL_SUBQUERIES,
        value: InfoValue::Bitmask(
            odbc::SQL_SQ_COMPARISON
                | odbc::SQL_SQ_EXISTS
                | odbc::SQL_SQ_IN
                | odbc::SQL_SQ_QUANTIFIED
                | odbc::SQL_SQ_CORRELATED_SUBQUERIES,
        ),
    },
    InfoEntry {
        info_type: odbc::SQL_UNION,
        value: InfoValue::Bitmask(odbc::SQL_U_UNION | odbc::SQL_U_UNION_ALL),
    },
    InfoEntry {
        info_type: odbc::SQL_MAX_CATALOG_NAME_LEN,
        value: InfoValue::U16(MAX_IDENTIFIER_LEN),
    },
    InfoEntry {
        info_type: odbc::SQL_MAX_COLUMNS_IN_GROUP_BY,
        value: InfoValue::U16(NO_STATED_U16_LIMIT),
    },
    InfoEntry {
        info_type: odbc::SQL_MAX_COLUMNS_IN_INDEX,
        value: InfoValue::U16(SQL_SERVER_MAX_INDEX_COLUMNS),
    },
    InfoEntry {
        info_type: odbc::SQL_MAX_COLUMNS_IN_ORDER_BY,
        value: InfoValue::U16(NO_STATED_U16_LIMIT),
    },
    InfoEntry {
        info_type: odbc::SQL_MAX_COLUMNS_IN_SELECT,
        value: InfoValue::U16(SQL_SERVER_MAX_SELECT_COLUMNS),
    },
    InfoEntry {
        info_type: odbc::SQL_MAX_COLUMNS_IN_TABLE,
        value: InfoValue::U16(SQL_SERVER_MAX_TABLE_COLUMNS),
    },
    InfoEntry {
        info_type: odbc::SQL_MAX_IDENTIFIER_LEN,
        value: InfoValue::U16(MAX_IDENTIFIER_LEN),
    },
    InfoEntry {
        info_type: odbc::SQL_MAX_ROW_SIZE,
        value: InfoValue::U32(SQL_SERVER_MAX_ROW_SIZE),
    },
    InfoEntry {
        info_type: odbc::SQL_MAX_TABLES_IN_SELECT,
        value: InfoValue::U16(SQL_SERVER_MAX_TABLES_IN_SELECT),
    },
    InfoEntry {
        info_type: odbc::SQL_MAX_USER_NAME_LEN,
        value: InfoValue::U16(MAX_IDENTIFIER_LEN),
    },
    InfoEntry {
        info_type: odbc::SQL_FETCH_DIRECTION,
        value: InfoValue::Bitmask(odbc::SQL_FD_FETCH_NEXT),
    },
    InfoEntry {
        info_type: odbc::SQL_POSITIONED_STATEMENTS,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    InfoEntry {
        info_type: odbc::SQL_SCROLL_CONCURRENCY,
        value: InfoValue::Bitmask(odbc::SQL_SCCO_READ_ONLY),
    },
    InfoEntry {
        info_type: odbc::SQL_STATIC_SENSITIVITY,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    InfoEntry {
        info_type: odbc::SQL_XOPEN_CLI_YEAR,
        value: InfoValue::String("1995"),
    },
    // AB#47996: remaining ODBC 3.x information types. Values from msodbcsql's
    // `SQLGetInfoTable` (`sqlcinfo.cpp`); confirmed against retail 18.6.2.1 by
    // the compare-leg E2E suite.
    InfoEntry {
        info_type: odbc::SQL_ROW_UPDATES,
        value: InfoValue::String("N"),
    },
    InfoEntry {
        info_type: odbc::SQL_MAX_ROW_SIZE_INCLUDES_LONG,
        value: InfoValue::String("N"),
    },
    // `SQL_INTEGRITY` (`SQL_ODBC_SQL_OPT_IEF`): msodbcsql's code path forces "Y".
    InfoEntry {
        info_type: odbc::SQL_INTEGRITY,
        value: InfoValue::String("Y"),
    },
    InfoEntry {
        info_type: odbc::SQL_ACTIVE_ENVIRONMENTS,
        value: InfoValue::U16(NO_STATED_U16_LIMIT),
    },
    InfoEntry {
        info_type: odbc::SQL_FILE_USAGE,
        value: InfoValue::U16(SQL_FILE_NOT_SUPPORTED),
    },
    InfoEntry {
        info_type: odbc::SQL_CATALOG_LOCATION,
        value: InfoValue::U16(SQL_QL_START),
    },
    InfoEntry {
        info_type: odbc::SQL_NON_NULLABLE_COLUMNS,
        value: InfoValue::U16(SQL_NNC_NON_NULL),
    },
    InfoEntry {
        info_type: odbc::SQL_MAX_CURSOR_NAME_LEN,
        value: InfoValue::U16(SQL_SERVER_MAX_CURSOR_NAME_LEN),
    },
    InfoEntry {
        info_type: odbc::SQL_MAX_PROCEDURE_NAME_LEN,
        value: InfoValue::U16(SQL_SERVER_MAX_PROCEDURE_NAME_LEN),
    },
    InfoEntry {
        info_type: odbc::SQL_MAX_INDEX_SIZE,
        value: InfoValue::U32(SQL_SERVER_MAX_INDEX_SIZE),
    },
    InfoEntry {
        info_type: odbc::SQL_MAX_ASYNC_CONCURRENT_STATEMENTS,
        // Async is not implemented (`SQL_ASYNC_MODE` is `SQL_AM_NONE`), so this
        // reports no async statements rather than msodbcsql's 1 (see plan.md
        // Phase 15). Capability ledger, not parity.
        value: InfoValue::U32(0),
    },
    InfoEntry {
        info_type: odbc::SQL_ODBC_INTERFACE_CONFORMANCE,
        // 0, not msodbcsql's Level 2 nor even Core: the Core interface set
        // requires `SQLGetCursorName` / `SQLSetCursorName`, which are
        // unimplemented (and correctly absent from `SQLGetFunctions`), so no
        // named conformance level is fully met. Rises to Core when cursor-name
        // support lands with Phase 10 (`docs/odbc-escape-sequences-plan.md`).
        // Capability ledger, not parity.
        value: InfoValue::U32(0),
    },
    InfoEntry {
        info_type: odbc::SQL_STANDARD_CLI_CONFORMANCE,
        value: InfoValue::Bitmask(SQL_SCC_ISO92_CLI),
    },
    InfoEntry {
        info_type: odbc::SQL_AGGREGATE_FUNCTIONS,
        value: InfoValue::Bitmask(SQL_AF_ALL),
    },
    InfoEntry {
        info_type: odbc::SQL_INDEX_KEYWORDS,
        value: InfoValue::Bitmask(SQL_IK_ALL),
    },
    InfoEntry {
        info_type: odbc::SQL_INSERT_STATEMENT,
        value: InfoValue::Bitmask(
            SQL_IS_INSERT_LITERALS | SQL_IS_INSERT_SEARCHED | SQL_IS_SELECT_INTO,
        ),
    },
    InfoEntry {
        info_type: odbc::SQL_INFO_SCHEMA_VIEWS,
        value: InfoValue::Bitmask(SQL_INFO_SCHEMA_VIEWS_MASK),
    },
    // SQLSetPos is not implemented (planned in Phase 10), so this driver
    // advertises no positioned operations or lock types.
    InfoEntry {
        info_type: odbc::SQL_LOCK_TYPES,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    InfoEntry {
        info_type: odbc::SQL_POS_OPERATIONS,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    InfoEntry {
        info_type: odbc::SQL_CREATE_SCHEMA,
        value: InfoValue::Bitmask(SQL_CS_CREATE_SCHEMA | SQL_CS_AUTHORIZATION),
    },
    InfoEntry {
        info_type: odbc::SQL_CREATE_TABLE,
        value: InfoValue::Bitmask(SQL_CT_CREATE_TABLE),
    },
    InfoEntry {
        info_type: odbc::SQL_DROP_TABLE,
        value: InfoValue::Bitmask(SQL_DT_DROP_TABLE),
    },
    InfoEntry {
        info_type: odbc::SQL_DROP_VIEW,
        value: InfoValue::Bitmask(SQL_DV_DROP_VIEW),
    },
    // Features SQL Server does not implement: ODBC specifies a zero mask, not a
    // failure.
    InfoEntry {
        info_type: odbc::SQL_ALTER_DOMAIN,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    InfoEntry {
        info_type: odbc::SQL_DATETIME_LITERALS,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    InfoEntry {
        info_type: odbc::SQL_CREATE_CHARACTER_SET,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    InfoEntry {
        info_type: odbc::SQL_CREATE_COLLATION,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    InfoEntry {
        info_type: odbc::SQL_CREATE_DOMAIN,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    InfoEntry {
        info_type: odbc::SQL_CREATE_TRANSLATION,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    InfoEntry {
        info_type: odbc::SQL_DROP_ASSERTION,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    InfoEntry {
        info_type: odbc::SQL_DROP_CHARACTER_SET,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    InfoEntry {
        info_type: odbc::SQL_DROP_COLLATION,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    InfoEntry {
        info_type: odbc::SQL_DROP_DOMAIN,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    InfoEntry {
        info_type: odbc::SQL_DROP_SCHEMA,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    InfoEntry {
        info_type: odbc::SQL_DROP_TRANSLATION,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    // Conversion-support masks (msodbcsql `SQL_CVT_*_SPT`).
    InfoEntry {
        info_type: odbc::SQL_CONVERT_BIGINT,
        value: InfoValue::Bitmask(CVT_NUMBER_SPT),
    },
    InfoEntry {
        info_type: odbc::SQL_CONVERT_BINARY,
        value: InfoValue::Bitmask(CVT_BINARY_SPT),
    },
    InfoEntry {
        info_type: odbc::SQL_CONVERT_BIT,
        value: InfoValue::Bitmask(CVT_BIT_SPT),
    },
    InfoEntry {
        info_type: odbc::SQL_CONVERT_CHAR,
        value: InfoValue::Bitmask(CVT_CHAR_SPT),
    },
    InfoEntry {
        info_type: odbc::SQL_CONVERT_DECIMAL,
        value: InfoValue::Bitmask(CVT_NUMBER_SPT),
    },
    InfoEntry {
        info_type: odbc::SQL_CONVERT_FLOAT,
        value: InfoValue::Bitmask(CVT_APXNUM_SPT),
    },
    InfoEntry {
        info_type: odbc::SQL_CONVERT_GUID,
        value: InfoValue::Bitmask(CVT_GUID_SPT),
    },
    InfoEntry {
        info_type: odbc::SQL_CONVERT_INTEGER,
        value: InfoValue::Bitmask(CVT_NUMBER_SPT),
    },
    InfoEntry {
        info_type: odbc::SQL_CONVERT_LONGVARBINARY,
        value: InfoValue::Bitmask(CVT_LONGVARBINARY_SPT),
    },
    InfoEntry {
        info_type: odbc::SQL_CONVERT_LONGVARCHAR,
        value: InfoValue::Bitmask(CVT_LONGVARCHAR_SPT),
    },
    InfoEntry {
        info_type: odbc::SQL_CONVERT_NUMERIC,
        value: InfoValue::Bitmask(CVT_NUMBER_SPT),
    },
    InfoEntry {
        info_type: odbc::SQL_CONVERT_REAL,
        value: InfoValue::Bitmask(CVT_APXNUM_SPT),
    },
    InfoEntry {
        info_type: odbc::SQL_CONVERT_SMALLINT,
        value: InfoValue::Bitmask(CVT_NUMBER_SPT),
    },
    InfoEntry {
        info_type: odbc::SQL_CONVERT_TIMESTAMP,
        value: InfoValue::Bitmask(CVT_TIMESTAMP_SPT),
    },
    InfoEntry {
        info_type: odbc::SQL_CONVERT_TINYINT,
        value: InfoValue::Bitmask(CVT_NUMBER_SPT),
    },
    InfoEntry {
        info_type: odbc::SQL_CONVERT_VARBINARY,
        value: InfoValue::Bitmask(CVT_BINARY_SPT),
    },
    InfoEntry {
        info_type: odbc::SQL_CONVERT_VARCHAR,
        value: InfoValue::Bitmask(CVT_VARCHAR_SPT),
    },
    InfoEntry {
        info_type: odbc::SQL_CONVERT_WCHAR,
        value: InfoValue::Bitmask(CVT_CHAR_SPT),
    },
    InfoEntry {
        info_type: odbc::SQL_CONVERT_WVARCHAR,
        value: InfoValue::Bitmask(CVT_VARCHAR_SPT),
    },
    InfoEntry {
        info_type: odbc::SQL_CONVERT_WLONGVARCHAR,
        value: InfoValue::Bitmask(CVT_LONGVARCHAR_SPT),
    },
    // msodbcsql reports a zero conversion mask for these targets; kept for
    // parity, not a statement about SQL Server's own CAST/CONVERT support.
    InfoEntry {
        info_type: odbc::SQL_CONVERT_DATE,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    InfoEntry {
        info_type: odbc::SQL_CONVERT_DOUBLE,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    InfoEntry {
        info_type: odbc::SQL_CONVERT_TIME,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    InfoEntry {
        info_type: odbc::SQL_CONVERT_INTERVAL_DAY_TIME,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    InfoEntry {
        info_type: odbc::SQL_CONVERT_INTERVAL_YEAR_MONTH,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    // `SQL_CREATE_VIEW` and the SQL-92 capability masks (msodbcsql `SQLGetInfoTable`).
    InfoEntry {
        info_type: odbc::SQL_CREATE_VIEW,
        value: InfoValue::Bitmask(SQL_CREATE_VIEW_MASK),
    },
    InfoEntry {
        info_type: odbc::SQL_SQL92_GRANT,
        value: InfoValue::Bitmask(SQL_SG_WITH_GRANT_OPTION),
    },
    InfoEntry {
        info_type: odbc::SQL_SQL92_REVOKE,
        value: InfoValue::Bitmask(SQL_SR_GRANT_OPTION_FOR),
    },
    InfoEntry {
        info_type: odbc::SQL_SQL92_PREDICATES,
        value: InfoValue::Bitmask(SQL_SQL92_PREDICATES_SPT),
    },
    InfoEntry {
        info_type: odbc::SQL_SQL92_RELATIONAL_JOIN_OPERATORS,
        value: InfoValue::Bitmask(SQL_SQL92_RELATIONAL_JOIN_OPERATORS_SPT),
    },
    InfoEntry {
        info_type: odbc::SQL_SQL92_ROW_VALUE_CONSTRUCTOR,
        value: InfoValue::Bitmask(SQL_SQL92_ROW_VALUE_CONSTRUCTOR_SPT),
    },
    InfoEntry {
        info_type: odbc::SQL_SQL92_STRING_FUNCTIONS,
        value: InfoValue::Bitmask(SQL_SQL92_STRING_FUNCTIONS_SPT),
    },
    InfoEntry {
        info_type: odbc::SQL_SQL92_VALUE_EXPRESSIONS,
        value: InfoValue::Bitmask(SQL_SQL92_VALUE_EXPRESSIONS_SPT),
    },
    InfoEntry {
        info_type: odbc::SQL_SQL92_DATETIME_FUNCTIONS,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    InfoEntry {
        info_type: odbc::SQL_SQL92_FOREIGN_KEY_DELETE_RULE,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    InfoEntry {
        info_type: odbc::SQL_SQL92_FOREIGN_KEY_UPDATE_RULE,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
    InfoEntry {
        info_type: odbc::SQL_SQL92_NUMERIC_VALUE_FUNCTIONS,
        value: InfoValue::Bitmask(NO_CAPABILITIES),
    },
];

/// `SQL_KEYWORDS`: SQL Server reserved words that are *not* in the ODBC
/// interoperable list, which is the only thing ODBC asks this to report.
/// Verbatim from msodbcsql18 18.6.2.1.
const SQL_SERVER_KEYWORDS: &str = "BACKUP,BREAK,BROWSE,BULK,CHECKPOINT,CLUSTERED,COMMITTED,\
COMPUTE,CONFIRM,CONTROLROW,DATABASE,DBCC,DISK,DISTRIBUTED,DUMMY,ERRLVL,ERROREXIT,EXIT,FILE,\
FILLFACTOR,FLOPPY,HOLDLOCK,IDENTITY_INSERT,IDENTITYCOL,IF,KILL,LINENO,MERGE,MIRROREXIT,\
NONCLUSTERED,OFF,OFFSETS,ONCE,OVER,PERCENT,PERM,PERMANENT,PLAN,PRINT,PROC,PROCESSEXIT,RAISERROR,\
READ,READTEXT,RECONFIGURE,REPEATABLE,RESTORE,RETURN,ROWCOUNT,RULE,SAVE,SERIALIZABLE,SETUSER,\
SHUTDOWN,STATISTICS,TAPE,TEMP,TEXTSIZE,TOP,TRAN,TRIGGER,TRUNCATE,TSEQUEL,UNCOMMITTED,UPDATETEXT,\
USE,WAITFOR,WHILE,WRITETEXT";

/// `SQL_SPECIAL_CHARACTERS`: characters legal in an unquoted SQL Server
/// identifier beyond `A-Z`, `0-9`, and `_`. `#` and `$` plus the Latin-1
/// letters, which excludes U+00D7 (multiplication sign) and U+00F7 (division
/// sign) because those are symbols, not letters. `@` is absent: it is only legal
/// as the *first* character of a variable or parameter name.
/// `special_characters_match_msodbcsql` pins the exact code points.
const SQL_SERVER_SPECIAL_CHARACTERS: &str = "#$\u{c0}\u{c1}\u{c2}\u{c3}\u{c4}\u{c5}\u{c6}\u{c7}\u{c8}\u{c9}\u{ca}\u{cb}\u{cc}\u{cd}\
\u{ce}\u{cf}\u{d0}\u{d1}\u{d2}\u{d3}\u{d4}\u{d5}\u{d6}\u{d8}\u{d9}\u{da}\u{db}\u{dc}\u{dd}\
\u{de}\u{df}\u{e0}\u{e1}\u{e2}\u{e3}\u{e4}\u{e5}\u{e6}\u{e7}\u{e8}\u{e9}\u{ea}\u{eb}\u{ec}\
\u{ed}\u{ee}\u{ef}\u{f0}\u{f1}\u{f2}\u{f3}\u{f4}\u{f5}\u{f6}\u{f8}\u{f9}\u{fa}\u{fb}\u{fc}\
\u{fd}\u{fe}\u{ff}";

/// Returns driver/data-source metadata for a connection.
///
/// # Safety
/// - `connection_handle` must be a valid DBC handle from `SQLAllocHandle`.
/// - `info_value_ptr` and `string_length_ptr` must satisfy the ODBC contract
///   for the requested `info_type`.
pub(crate) unsafe fn sql_get_info_w(
    connection_handle: SqlHandle,
    info_type: SqlUSmallInt,
    info_value_ptr: SqlPointer,
    buffer_length: SqlSmallInt,
    string_length_ptr: *mut SqlSmallInt,
) -> SqlReturn {
    debug!(
        ?connection_handle,
        info_type,
        ?info_value_ptr,
        buffer_length,
        ?string_length_ptr,
        "SQLGetInfoW called",
    );

    crate::ffi_entry!("SQLGetInfoW", unsafe {
        sql_get_info_w_impl(
            connection_handle,
            info_type,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
        )
    })
}

/// # Safety
/// `connection_handle` must be null or point to a live `DbcHandle`.
/// `info_value_ptr`, when non-null, must be writable for `buffer_length` bytes
/// for string information or for one value of the requested numeric information
/// type. `string_length_ptr`, when non-null, must be writable for one
/// `SqlSmallInt`.
unsafe fn sql_get_info_w_impl(
    connection_handle: SqlHandle,
    info_type: SqlUSmallInt,
    info_value_ptr: SqlPointer,
    buffer_length: SqlSmallInt,
    string_length_ptr: *mut SqlSmallInt,
) -> SqlReturn {
    if connection_handle.is_null() {
        error!("SQLGetInfoW: connection_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let dbc = unsafe { handle_from_raw::<DbcHandle>(connection_handle) };
    debug_assert_eq!(
        dbc.object_type,
        HandleType::Dbc,
        "SQLGetInfoW: handle is not a DBC"
    );
    sql_get_info_w_safe(
        dbc,
        info_type,
        info_value_ptr,
        buffer_length,
        string_length_ptr,
    )
}

fn sql_get_info_w_safe(
    dbc: &DbcHandle,
    info_type: SqlUSmallInt,
    info_value_ptr: SqlPointer,
    buffer_length: SqlSmallInt,
    string_length_ptr: *mut SqlSmallInt,
) -> SqlReturn {
    // `SQL_USER_NAME` is the one information type whose value can require a
    // server round trip, and the DBC mutex must not be held across one. It is
    // resolved between two short critical sections rather than inside the single
    // one the other types share: the first does the entry-point bookkeeping
    // every ODBC call owes — clearing the previous call's diagnostics and
    // zeroing the output length — and rejects an invalid buffer before anything
    // reaches the wire; the second writes the answer.
    if info_type == SQL_USER_NAME {
        {
            let Ok(mut state) = dbc.inner.lock() else {
                error!("SQLGetInfoW: dbc mutex poisoned");
                return SQL_ERROR;
            };
            free_errors(&mut state);
            unsafe { write_if_some(string_length_ptr, 0) };
            // Checked here as well as in `write_wide_str` so a caller that
            // passes a negative length gets `HY090` without the driver first
            // issuing — and caching — a query on its behalf.
            if buffer_length < 0 {
                error!(buffer_length, "SQLGetInfoW: negative buffer length");
                post_diag(&mut state, ERR_INVALID_STRING_OR_BUFFER_LENGTH);
                return SQL_ERROR;
            }
        }

        let value = database_user_name(dbc);

        let Ok(mut state) = dbc.inner.lock() else {
            error!("SQLGetInfoW: dbc mutex poisoned");
            return SQL_ERROR;
        };
        return write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            &value,
        );
    }

    let Ok(mut state) = dbc.inner.lock() else {
        error!("SQLGetInfoW: dbc mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut state);

    unsafe { write_if_some(string_length_ptr, 0) };

    match info_type {
        SQL_DATA_SOURCE_NAME | SQL_SERVER_NAME => {
            let value = if info_type == SQL_DATA_SOURCE_NAME {
                state.identity.data_source_name.clone()
            } else {
                state.identity.server_name.clone()
            };
            write_wide_str(
                &mut state,
                info_value_ptr,
                buffer_length,
                string_length_ptr,
                &value,
            )
        }
        SQL_DATABASE_NAME => {
            let database = resolved_current_catalog(&state);
            write_wide_str(
                &mut state,
                info_value_ptr,
                buffer_length,
                string_length_ptr,
                &database,
            )
        }
        odbc::SQL_COLLATION_SEQ => {
            // Resolved from the live client, not snapshotted: the database
            // collation (hence code page) changes on `USE` / catalog switch.
            // While a data-at-execution sequence owns the client, fall back to
            // the value cached when that execution claimed it.
            let collation = match state.client.as_ref() {
                Some(client) => {
                    collation_seq_string(client.collation_code_page(), client.char_set())
                }
                None => collation_seq_string(
                    state.last_collation_code_page,
                    state.last_char_set.as_deref(),
                ),
            };
            write_wide_str(
                &mut state,
                info_value_ptr,
                buffer_length,
                string_length_ptr,
                &collation,
            )
        }
        SQL_MAX_DRIVER_CONNECTIONS => {
            // 0 means "no stated limit" per ODBC.
            write_u16(info_value_ptr, 0, string_length_ptr)
        }
        SQL_ACTIVE_STATEMENTS => write_u16(info_value_ptr, 0, string_length_ptr),
        SQL_DRIVER_NAME => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            driver_name(),
        ),
        SQL_DRIVER_VER => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            "18.6.2.1",
        ),
        SQL_DRIVER_ODBC_VER | SQL_ODBC_VER => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            "03.80",
        ),
        SQL_ODBC_API_CONFORMANCE => write_u16(info_value_ptr, SQL_OAC_LEVEL2, string_length_ptr),
        SQL_ODBC_SQL_CONFORMANCE => write_u16(info_value_ptr, SQL_OSC_CORE, string_length_ptr),
        SQL_CURSOR_COMMIT_BEHAVIOR => write_u16(info_value_ptr, SQL_CB_CLOSE, string_length_ptr),
        SQL_CURSOR_ROLLBACK_BEHAVIOR => write_u16(info_value_ptr, SQL_CB_CLOSE, string_length_ptr),
        // Transactions cover both DML and DDL on SQL Server (`sqlcinfo.cpp`).
        SQL_TXN_CAPABLE => write_u16(info_value_ptr, SQL_TC_ALL, string_length_ptr),
        SQL_DEFAULT_TXN_ISOLATION => {
            write_u32(info_value_ptr, SQL_TXN_READ_COMMITTED, string_length_ptr)
        }
        SQL_TXN_ISOLATION_OPTION => write_u32(
            info_value_ptr,
            SQL_TXN_ISOLATION_OPTION_SPT,
            string_length_ptr,
        ),
        // A connection supports only one transaction at a time, but several
        // connections may each hold one simultaneously.
        SQL_MULTIPLE_ACTIVE_TXN => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            "Y",
        ),
        SQL_GETDATA_EXTENSIONS => write_u32(
            info_value_ptr,
            SQL_GD_ANY_COLUMN | SQL_GD_ANY_ORDER,
            string_length_ptr,
        ),
        SQL_DBMS_NAME => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            "Microsoft SQL Server",
        ),
        SQL_DBMS_VER => {
            // ODBC reports SQL_DBMS_VER as "##.##.####" (major.minor.build).
            // Use the version negotiated at login; fall back to a neutral
            // placeholder when the connection has no reported version yet.
            let version = state
                .client
                .as_ref()
                .and_then(|c| c.server_version())
                .map(|v| format!("{:02}.{:02}.{:04}", v.major, v.minor, v.build))
                .unwrap_or_else(|| "00.00.0000".to_string());
            write_wide_str(
                &mut state,
                info_value_ptr,
                buffer_length,
                string_length_ptr,
                &version,
            )
        }
        SQL_IDENTIFIER_QUOTE_CHAR => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            "\"",
        ),
        SQL_NEED_LONG_DATA_LEN => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            "N",
        ),
        SQL_ASYNC_DBC_FUNCTIONS => {
            write_u32(info_value_ptr, SQL_ASYNC_DBC_NOT_CAPABLE, string_length_ptr)
        }
        SQL_ASYNC_NOTIFICATION => write_u32(
            info_value_ptr,
            SQL_ASYNC_NOTIFICATION_NOT_CAPABLE,
            string_length_ptr,
        ),
        SQL_DM_VER => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            "03.80.0000",
        ),
        SQL_SQL_CONFORMANCE => write_u32(info_value_ptr, SQL_SC_SQL92_ENTRY, string_length_ptr),
        SQL_KEYWORDS => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            SQL_SERVER_KEYWORDS,
        ),
        SQL_SPECIAL_CHARACTERS => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            SQL_SERVER_SPECIAL_CHARACTERS,
        ),
        SQL_CATALOG_TERM => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            "database",
        ),
        SQL_CATALOG_NAME_SEPARATOR => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            ".",
        ),
        // msodbcsql reports the pre-ODBC-3 term, and applications building
        // three-part names key off it, so parity wins over the modern spelling.
        SQL_SCHEMA_TERM => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            "owner",
        ),
        // SQL Server has procedures. The other half of ODBC's definition -- the
        // `{call ...}` invocation escape -- is still outstanding (AB#46384).
        SQL_PROCEDURES => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            "Y",
        ),
        SQL_MAX_COLUMN_NAME_LEN | SQL_MAX_SCHEMA_NAME_LEN | SQL_MAX_TABLE_NAME_LEN => {
            write_u16(info_value_ptr, MAX_IDENTIFIER_LEN, string_length_ptr)
        }
        // The resolved value for the live connection when connected, else the
        // app-set attribute/default — mirrors `SQL_ATTR_PACKET_SIZE`'s own
        // get-side fallback. Never the ENVCHANGE-negotiated one, matching
        // msodbcsql: its own `SQLGetConnectAttr`/`SQLGetInfo` read the single
        // slot the LOGIN7 request was built from, which nothing overwrites
        // post-negotiation.
        odbc::SQL_MAX_BINARY_LITERAL_LEN
        | odbc::SQL_MAX_CHAR_LITERAL_LEN
        | SQL_MAX_STATEMENT_LEN => write_u32(
            info_value_ptr,
            max_statement_len(state.effective_packet_size.unwrap_or(state.packet_size)),
            string_length_ptr,
        ),
        // The `{fn ...}` escape is translated (validated and forwarded; SQL
        // Server parses it natively), so these advertise the sets msodbcsql
        // advertises rather than "none supported".
        SQL_NUMERIC_FUNCTIONS => write_u32(
            info_value_ptr,
            SQL_NUMERIC_FUNCTIONS_SUPPORTED,
            string_length_ptr,
        ),
        SQL_STRING_FUNCTIONS => write_u32(
            info_value_ptr,
            SQL_STRING_FUNCTIONS_SUPPORTED,
            string_length_ptr,
        ),
        SQL_SYSTEM_FUNCTIONS => write_u32(
            info_value_ptr,
            SQL_SYSTEM_FUNCTIONS_SUPPORTED,
            string_length_ptr,
        ),
        SQL_TIMEDATE_FUNCTIONS => write_u32(
            info_value_ptr,
            SQL_TIMEDATE_FUNCTIONS_SUPPORTED,
            string_length_ptr,
        ),
        SQL_CONVERT_FUNCTIONS => write_u32(
            info_value_ptr,
            SQL_CONVERT_FUNCTIONS_SUPPORTED,
            string_length_ptr,
        ),
        SQL_TIMEDATE_ADD_INTERVALS | SQL_TIMEDATE_DIFF_INTERVALS => write_u32(
            info_value_ptr,
            SQL_TIMEDATE_INTERVALS_SUPPORTED,
            string_length_ptr,
        ),
        SQL_OJ_CAPABILITIES => write_u32(
            info_value_ptr,
            SQL_OJ_CAPABILITIES_SUPPORTED,
            string_length_ptr,
        ),
        // Deprecated ODBC 1.0 spelling of the same capability: "F" is full
        // outer join support.
        SQL_OUTER_JOINS => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            "F",
        ),
        SQL_LIKE_ESCAPE_CLAUSE => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            "Y",
        ),
        // Matching msodbcsql18: SQL Server grants the catalog views to public,
        // so what `SQLTables` / `SQLProcedures` list back is what the caller may
        // use.
        SQL_ACCESSIBLE_TABLES | SQL_ACCESSIBLE_PROCEDURES | SQL_EXPRESSIONS_IN_ORDERBY => {
            write_wide_str(
                &mut state,
                info_value_ptr,
                buffer_length,
                string_length_ptr,
                "Y",
            )
        }
        // msodbcsql queries DATABASEPROPERTYEX and caches the answer. This
        // driver has no internal metadata-query facility yet, so exact
        // read-only-database parity is deferred to AB#47996.
        SQL_DATA_SOURCE_READ_ONLY => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            "N",
        ),
        SQL_PARAM_ARRAY_ROW_COUNTS => {
            write_u32(info_value_ptr, SQL_PARC_NO_BATCH, string_length_ptr)
        }
        SQL_PARAM_ARRAY_SELECTS => write_u32(info_value_ptr, SQL_PAS_BATCH, string_length_ptr),
        _ => match STATIC_INFO
            .iter()
            .find(|entry| entry.info_type == info_type)
        {
            Some(entry) => match entry.value {
                InfoValue::String(value) => write_wide_str(
                    &mut state,
                    info_value_ptr,
                    buffer_length,
                    string_length_ptr,
                    value,
                ),
                InfoValue::U16(value) => write_u16(info_value_ptr, value, string_length_ptr),
                InfoValue::U32(value) | InfoValue::Bitmask(value) => {
                    write_u32(info_value_ptr, value, string_length_ptr)
                }
            },
            None => {
                error!(info_type, "SQLGetInfoW: unsupported info type");
                post_diag(&mut state, ERR_INVALID_INFO_TYPE);
                SQL_ERROR
            }
        },
    }
}

/// Answers `SQL_USER_NAME`: the database principal reported by `USER_NAME()`,
/// looked up lazily on first use and cached against the catalog it belongs to.
///
/// **Never fails and never posts a diagnostic.** msodbcsql's lookup
/// (`RefreshShilohUDTCache`, `sqlccmd.cpp:10337`) returns `void`: a busy
/// connection, a failed query, and a NULL or absent row all leave the cached
/// `conninfo.DBUserName` untouched, and `SQLGetInfo` then reports whatever that
/// buffer holds (`sqlcinfo.cpp:1189-1192`). Surfacing an error instead would
/// make `SQL_USER_NAME` the only information type that can fail on a healthy
/// connection, and would break the ODBC guarantee — which the driver's own
/// `WorksWithAnOpenCursorAndLeavesItUsable` E2E case pins — that `SQLGetInfo`
/// is answerable while a cursor is open.
///
/// Neither the internal query's INFO messages nor its errors reach the
/// application's diagnostics. They belong to no application statement, which is
/// also how msodbcsql treats them — its lookup runs on the hidden driver
/// statement (`lpdbcIn->lpstmtDvr`) and any records are freed with it. Nothing
/// is drained here to achieve that: `TdsClient::begin_command` already clears
/// `info_messages` at the top of every command, so the next statement to use
/// this connection starts from a clean slate on its own.
fn database_user_name(dbc: &DbcHandle) -> String {
    // Holding a stale entry rather than clearing it is deliberate: msodbcsql
    // leaves `DBUserName` untouched on a failed refresh and keeps answering
    // from it, retrying on the next call because `ExecImmediate` returning
    // `SQL_ERROR` skips the `CONN_ST_REFRESH_UDT` clear (`sqlccmd.cpp:10387`).
    let (fallback, timeout_secs) = {
        let Ok(state) = dbc.inner.lock() else {
            error!("SQLGetInfoW(SQL_USER_NAME): dbc mutex poisoned");
            return String::new();
        };
        if state.connection_state != ConnectionState::Connected {
            return String::new();
        }
        let stale = match &state.database_user_name {
            // Keyed by catalog, so a database change refreshes the answer
            // whether it arrived through `SQL_ATTR_CURRENT_CATALOG` or through
            // raw T-SQL the server reports with an ENVCHANGE — the same two
            // routes that set msodbcsql's `CONN_ST_REFRESH_UDT`
            // (`sqlctokn.cpp:2881`). While another statement holds the client
            // the catalog cannot be read, so the entry is taken as current.
            Some(cached)
                if state.client.as_ref().is_none_or(|client| {
                    client.database().eq_ignore_ascii_case(&cached.catalog)
                }) =>
            {
                return cached.value.clone();
            }
            Some(cached) => cached.value.clone(),
            None => String::new(),
        };
        // `SQL_ATTR_CONNECTION_TIMEOUT`, which is what msodbcsql bounds this
        // same internal query with: `GetNetIOTimeOut` reads the millisecond
        // form of that attribute (`sqlcprot.h:1559-1563, 1605`) and
        // `RefreshShilohUDTCache` passes it to `ExecImmediate`
        // (`sqlccmd.cpp:10369, 10387`). `0` is the ODBC default and means no
        // deadline, which `ExecuteOptions::timeout_secs` spells the same way —
        // so an application that never sets the attribute is unaffected.
        //
        // There is no statement here to take `SQL_ATTR_QUERY_TIMEOUT` from:
        // `SQLGetInfo` is a connection-level call, and msodbcsql reaches for
        // the connection timeout for exactly that reason.
        (stale, state.connection_timeout)
    };

    let Some((mut client, generation)) = try_claim_idle_dbc_client(dbc) else {
        debug!("SQLGetInfoW(SQL_USER_NAME): connection is busy; reporting the cached value");
        return fallback;
    };
    let outcome = dbc
        .runtime
        .block_on(fetch_database_user_name(&mut client, timeout_secs));
    // Read after the query, not before: `execute` can transparently reconnect a
    // dropped session, and the reconnected one may land on the login's default
    // database. Keying the entry to where the answer actually came from keeps a
    // later read from matching it against a database it was never valid for.
    let catalog = client.database().to_string();

    let cache = match outcome {
        // The query ran. Cache the outcome even when it carried no name, so the
        // round trip is paid once per database rather than on every call:
        // msodbcsql clears `CONN_ST_REFRESH_UDT` the moment `ExecImmediate`
        // succeeds — *before* it fetches (`sqlccmd.cpp:10387-10397`) — so a
        // NULL or absent row stops it asking too.
        //
        // `USER_NAME()` is NULL for a login with no principal in the current
        // database, and `SQLGetData` leaves msodbcsql's buffer untouched for a
        // NULL, so the previous answer is what it goes on reporting.
        // `fallback` is that same previous answer.
        Ok(looked_up) => Some(looked_up.unwrap_or_else(|| fallback.clone())),
        // Draining or closing failed *after* the batch was accepted. msodbcsql
        // has already cleared its refresh flag by this point, so this stops
        // asking too and keeps reporting the previous answer.
        Err(failure) if failure.executed => {
            debug!(error = %failure.error, "SQLGetInfoW(SQL_USER_NAME): lookup failed after execution");
            Some(fallback.clone())
        }
        // The batch never ran, which is the only case that leaves the refresh
        // outstanding — so nothing is cached and the next call retries.
        Err(failure) => {
            debug!(error = %failure.error, "SQLGetInfoW(SQL_USER_NAME): execution failed; reporting the cached value");
            None
        }
    };

    publish_lookup(dbc, client, generation, catalog, cache, fallback)
}

/// Hands the claimed client back and installs the cache entry **in one critical
/// section**, so no concurrent operation can slip between the two.
///
/// Both orderings are unsafe if split. Releasing first lets another thread
/// claim the client, switch catalogs and clear the cache, after which this
/// thread's write would resurrect an entry the switch had just invalidated.
/// Caching first would briefly advertise a principal for a session whose client
/// is not back yet.
///
/// `generation` is the session this answer came from. A `SQLDisconnect` plus a
/// fresh `SQLDriverConnect` can complete while the lookup is in flight, and the
/// client in hand then belongs to a closed session: storing it would overwrite
/// the live one and attach the old principal to it. In that case the client is
/// dropped and nothing is reported, because nothing is known about the session
/// now on the handle.
///
/// Returns the value to report: the cached one when `cache` is `Some`,
/// `fallback` when the lookup produced nothing worth caching, and the empty
/// string when the session turned over and nothing is known about its
/// replacement.
fn publish_lookup(
    dbc: &DbcHandle,
    client: TdsClient,
    generation: u64,
    catalog: String,
    cache: Option<String>,
    fallback: String,
) -> String {
    let mut state = match dbc.inner.lock() {
        Ok(state) => state,
        Err(mut poisoned) => {
            // The client cannot be stored, so the DBC must not keep claiming to
            // be connected — the same correction `release_dbc_client` makes.
            error!("SQLGetInfoW(SQL_USER_NAME): dbc mutex poisoned; marking DBC disconnected");
            poisoned.get_mut().connection_state = ConnectionState::Disconnected;
            return String::new();
        }
    };

    if state.session_generation != generation
        || state.connection_state != ConnectionState::Connected
    {
        debug!("SQLGetInfoW(SQL_USER_NAME): session replaced during the lookup; discarding it");
        drop(client);
        return String::new();
    }

    state.client = Some(client);
    match cache {
        Some(value) => {
            state.database_user_name = Some(CachedDatabaseUserName {
                catalog,
                value: value.clone(),
            });
            value
        }
        None => fallback,
    }
}

/// A `USER_NAME()` lookup that did not produce a value, and whether the batch
/// had been accepted by the server when it failed.
///
/// The distinction is the caching boundary: msodbcsql stops asking the moment
/// `ExecImmediate` succeeds (`sqlccmd.cpp:10387`), so only a failure to execute
/// leaves the refresh outstanding. Collapsing the two would make every later
/// `SQLGetInfo` re-issue the query after a mid-drain network blip.
struct LookupFailure {
    executed: bool,
    error: mssql_tds::error::Error,
}

/// Runs the `USER_NAME()` lookup on an already-claimed client and drains the
/// response.
///
/// msodbcsql issues `set implicit_transactions off select USER_NAME()` plus its
/// alias-type query in one `sp_executesql` (`sqlcstr.cpp:46`). The
/// `set implicit_transactions off` is there because msodbcsql implements
/// manual-commit with `SET IMPLICIT_TRANSACTIONS` and must keep this internal
/// query from opening a transaction (`sqlccmd.cpp` bug #656241); this driver
/// drives transactions with TDS transaction-manager requests and never enables
/// implicit transactions, so a bare batch already carries that guarantee. The
/// alias-type half has no counterpart here — there is no UDT cache to refresh —
/// which leaves just the `SELECT`.
/// `timeout_secs` is `SQL_ATTR_CONNECTION_TIMEOUT`, with `0` meaning no
/// deadline; see the call site for why that is the attribute msodbcsql bounds
/// this query with.
async fn fetch_database_user_name(
    client: &mut TdsClient,
    timeout_secs: u32,
) -> Result<Option<String>, LookupFailure> {
    if let Err(error) = client
        .execute(
            DATABASE_USER_NAME_QUERY.to_string(),
            ExecuteOptions::new().timeout_secs(timeout_secs),
        )
        .await
    {
        // Nothing was accepted, so there is normally no batch to close; a
        // partially-written one is drained anyway rather than left open.
        if client.has_open_batch() {
            let _ = client.close_query().await;
        }
        return Err(LookupFailure {
            executed: false,
            error,
        });
    }

    let read = async {
        if !client.on_rows() && client.has_open_batch() {
            client.advance_to_rows().await?;
        }
        let value = match client.next_row().await? {
            Some(row) => match row.first() {
                Some(ColumnValues::String(value)) => Some(value.to_utf8_string()),
                _ => None,
            },
            None => None,
        };
        Ok::<_, mssql_tds::error::Error>(value)
    }
    .await;

    // INVARIANT: the batch has to be closed even when the read above failed
    // part-way. Leaving the connection mid-result would fail every later
    // operation on it — the lookup is best-effort, but it must not cost the
    // application its connection.
    let closed = if client.has_open_batch() {
        client.close_query().await
    } else {
        Ok(())
    };

    // Everything from here on happened after the server accepted the batch.
    match read.and_then(|value| closed.map(|()| value)) {
        Ok(value) => Ok(value),
        Err(error) => Err(LookupFailure {
            executed: true,
            error,
        }),
    }
}

fn max_statement_len(packet_size: u32) -> u32 {
    // `packet_size` is `state.effective_packet_size.unwrap_or(state.packet_size)`.
    // It is either `0` (msodbcsql's "unspecified" sentinel, exempt from the
    // clamp — see `set_connect_attr.rs`) or clamped to `[MIN_PACKET_SIZE,
    // MAX_PACKET_SIZE]`, either directly by `set_connect_attr` or via
    // `context.packet_size` (clamped by `apply_connection_params`, then
    // copied into `state.effective_packet_size` in `driver_connect.rs` after
    // connecting — never zero once connected, since `seed_and_apply_connection_params`
    // leaves the context's own nonzero default in place for a zero seed), so
    // this can never actually overflow — kept saturating anyway as cheap
    // defense-in-depth against a future caller passing an unclamped value.
    //
    // A `0` here therefore reports `0`, matching msodbcsql: it computes this
    // same product directly from its stored packet-size dwOption
    // (sqlcinfo.cpp: `MAXSQLBLOCKSSPHINX * dwOptions[SQL_PACKET_SIZE]`), and
    // its clamp (sqlcmisc.cpp) only runs on a truthy `vParam`, so an
    // explicitly-set `0` is stored verbatim and yields the same `0` there —
    // not a resolved connection default.
    MAX_SQL_BLOCKS.saturating_mul(packet_size)
}

fn write_u16(
    info_value_ptr: SqlPointer,
    value: u16,
    string_length_ptr: *mut SqlSmallInt,
) -> SqlReturn {
    unsafe { write_if_some(info_value_ptr as *mut u16, value) };
    unsafe { write_if_some(string_length_ptr, std::mem::size_of::<u16>() as SqlSmallInt) };
    SQL_SUCCESS
}

fn write_u32(
    info_value_ptr: SqlPointer,
    value: u32,
    string_length_ptr: *mut SqlSmallInt,
) -> SqlReturn {
    unsafe { write_if_some(info_value_ptr as *mut u32, value) };
    unsafe { write_if_some(string_length_ptr, std::mem::size_of::<u32>() as SqlSmallInt) };
    SQL_SUCCESS
}

fn write_wide_str(
    state: &mut crate::handles::dbc::DbcState,
    info_value_ptr: SqlPointer,
    buffer_length: SqlSmallInt,
    string_length_ptr: *mut SqlSmallInt,
    value: &str,
) -> SqlReturn {
    if buffer_length < 0 {
        error!(buffer_length, "SQLGetInfoW: negative buffer length");
        post_diag(state, ERR_INVALID_STRING_OR_BUFFER_LENGTH);
        return SQL_ERROR;
    }

    let utf16: Vec<SqlWChar> = value.encode_utf16().collect();
    let full_byte_len = utf16.len().saturating_mul(std::mem::size_of::<SqlWChar>());
    let report_len = full_byte_len.min(SqlSmallInt::MAX as usize) as SqlSmallInt;
    unsafe { write_if_some(string_length_ptr, report_len) };

    if info_value_ptr.is_null() {
        return SQL_SUCCESS;
    }

    let cap_wchars = (buffer_length as usize) / std::mem::size_of::<SqlWChar>();
    let truncated = unsafe { copy_with_nul(info_value_ptr as *mut SqlWChar, cap_wchars, &utf16) };
    if truncated {
        post_diag(state, WARN_STRING_TRUNCATION);
        SQL_SUCCESS_WITH_INFO
    } else {
        SQL_SUCCESS
    }
}

fn driver_name() -> &'static str {
    env!("MSSQL_ODBC_ARTIFACT")
}

/// `SQL_COLLATION_SEQ` name for a server code page. msodbcsql names only these
/// three (`sqlctokn.cpp` `ENV_DATABASECOLLATION`) and leaves the rest empty.
fn collation_seq_name(code_page: Option<u16>) -> Option<&'static str> {
    match code_page {
        Some(1252) => Some("ISO 8859-1"),
        Some(850) => Some("Code page 850"),
        Some(437) => Some("Code page 437"),
        _ => None,
    }
}

/// The `SQL_COLLATION_SEQ` string for a database code page and optional legacy
/// `CHARACTER_SET` name: the code page's msodbcsql name, else the legacy
/// display name, else empty.
fn collation_seq_string(code_page: Option<u16>, char_set: Option<&str>) -> String {
    collation_seq_name(code_page)
        .map(str::to_string)
        .or_else(|| char_set.map(char_set_display_name))
        .unwrap_or_default()
}

/// The connection-level `SQL_COLLATION_SEQ` cache inputs for a live client: the
/// database code page, plus the legacy `CHARACTER_SET` name only when it is
/// needed to name a code page [`collation_seq_name`] does not cover. Keeping
/// the code page as a plain `u16` lets [`claim_connection`] refresh the cache
/// without allocating on the common named-code-page path.
///
/// [`claim_connection`]: crate::api::exec_common::claim_connection
pub(super) fn collation_cache_inputs(client: &TdsClient) -> (Option<u16>, Option<String>) {
    let code_page = client.collation_code_page();
    let char_set = if collation_seq_name(code_page).is_none() {
        client.char_set().map(str::to_string)
    } else {
        None
    };
    (code_page, char_set)
}

/// Maps a legacy `CHARACTER_SET` `ENVCHANGE` name to its `SQL_COLLATION_SEQ`
/// display form the way msodbcsql's `ENV_CHARSET` handler does
/// (`sqlctokn.cpp`): `iso_1` becomes `ISO 8859-1`, everything else becomes
/// `Code page <suffix>` after dropping the two-character prefix.
fn char_set_display_name(char_set: &str) -> String {
    if char_set.eq_ignore_ascii_case("iso_1") {
        "ISO 8859-1".to_string()
    } else if char_set.chars().count() > 2 {
        let suffix: String = char_set.chars().skip(2).collect();
        format!("Code page {suffix}")
    } else {
        char_set.to_string()
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::ptr;

    use super::*;
    use crate::api::odbc_types::SQL_NULL_HANDLE;
    use crate::test_support::TestHandles;

    fn get_u16(dbc: SqlHandle, info_type: SqlUSmallInt) -> (SqlReturn, u16, SqlSmallInt) {
        let mut val: u16 = 0xAAAA;
        let mut len: SqlSmallInt = -1;
        let rc = unsafe {
            sql_get_info_w(
                dbc,
                info_type,
                &mut val as *mut u16 as SqlPointer,
                std::mem::size_of::<u16>() as SqlSmallInt,
                &mut len,
            )
        };
        (rc, val, len)
    }

    fn get_u32(dbc: SqlHandle, info_type: SqlUSmallInt) -> (SqlReturn, u32, SqlSmallInt) {
        let mut val: u32 = 0xAAAA_AAAA;
        let mut len: SqlSmallInt = -1;
        let rc = unsafe {
            sql_get_info_w(
                dbc,
                info_type,
                &mut val as *mut u32 as SqlPointer,
                std::mem::size_of::<u32>() as SqlSmallInt,
                &mut len,
            )
        };
        (rc, val, len)
    }

    /// Fetches a wide-string info type into a buffer large enough for any value
    /// this driver returns, and decodes the reported byte length.
    fn get_wide_str(dbc: SqlHandle, info_type: SqlUSmallInt) -> (SqlReturn, String, SqlSmallInt) {
        let mut buf = [0u16; 2048];
        let mut len: SqlSmallInt = -1;
        let rc = unsafe {
            sql_get_info_w(
                dbc,
                info_type,
                buf.as_mut_ptr() as SqlPointer,
                (buf.len() * std::mem::size_of::<SqlWChar>()) as SqlSmallInt,
                &mut len,
            )
        };
        let n = if len < 0 { 0 } else { (len as usize) / 2 };
        (rc, String::from_utf16_lossy(&buf[..n]), len)
    }

    /// Every wide-string info type added for AB#47086, with the value measured
    /// from msodbcsql18 18.6.2.1 against SQL Server.
    const STRING_CASES: &[(SqlUSmallInt, &str)] = &[
        (SQL_KEYWORDS, SQL_SERVER_KEYWORDS),
        (SQL_SPECIAL_CHARACTERS, SQL_SERVER_SPECIAL_CHARACTERS),
        (SQL_CATALOG_TERM, "database"),
        (SQL_CATALOG_NAME_SEPARATOR, "."),
        (SQL_SCHEMA_TERM, "owner"),
        (SQL_PROCEDURES, "Y"),
        // The {oj ...} and {escape ...} escapes both reach the server.
        (SQL_OUTER_JOINS, "F"),
        (SQL_LIKE_ESCAPE_CLAUSE, "Y"),
        (SQL_ACCESSIBLE_TABLES, "Y"),
        (SQL_ACCESSIBLE_PROCEDURES, "Y"),
        (SQL_EXPRESSIONS_IN_ORDERBY, "Y"),
        (SQL_DATA_SOURCE_READ_ONLY, "N"),
    ];

    /// Info types the ODBC 3.x spec documents as `SQLUSMALLINT`-width
    /// (2 bytes), transcribed independently from `sql.h`/`sqlext.h` rather
    /// than read off `STATIC_INFO::value`'s variant. If a `STATIC_INFO`
    /// entry ever used the wrong `InfoValue` variant for its info type (e.g.
    /// `U32` for something the spec defines as `SQLUSMALLINT`), asserting
    /// against `entry.value`'s own variant would validate `table == table`
    /// and miss it; this independent list is what actually pins the width.
    const SPEC_U16_INFO_TYPES: &[SqlUSmallInt] = &[
        odbc::SQL_CONCAT_NULL_BEHAVIOR,
        odbc::SQL_NULL_COLLATION,
        odbc::SQL_CORRELATION_NAME,
        odbc::SQL_GROUP_BY,
        odbc::SQL_IDENTIFIER_CASE,
        odbc::SQL_QUOTED_IDENTIFIER_CASE,
        odbc::SQL_MAX_CATALOG_NAME_LEN,
        odbc::SQL_MAX_COLUMNS_IN_GROUP_BY,
        odbc::SQL_MAX_COLUMNS_IN_INDEX,
        odbc::SQL_MAX_COLUMNS_IN_ORDER_BY,
        odbc::SQL_MAX_COLUMNS_IN_SELECT,
        odbc::SQL_MAX_COLUMNS_IN_TABLE,
        odbc::SQL_MAX_IDENTIFIER_LEN,
        odbc::SQL_MAX_TABLES_IN_SELECT,
        odbc::SQL_MAX_USER_NAME_LEN,
        odbc::SQL_ACTIVE_ENVIRONMENTS,
        odbc::SQL_FILE_USAGE,
        odbc::SQL_CATALOG_LOCATION,
        odbc::SQL_NON_NULLABLE_COLUMNS,
        odbc::SQL_MAX_CURSOR_NAME_LEN,
        odbc::SQL_MAX_PROCEDURE_NAME_LEN,
    ];

    #[test]
    fn static_info_types_are_unique_and_report_typed_values() {
        let h = TestHandles::with_env_dbc();
        let mut seen = HashSet::new();

        assert_eq!(STATIC_INFO.len(), 122);
        for entry in STATIC_INFO {
            assert!(
                seen.insert(entry.info_type),
                "duplicate info_type {}",
                entry.info_type
            );
            let spec_says_u16 = SPEC_U16_INFO_TYPES.contains(&entry.info_type);
            match entry.value {
                InfoValue::U16(_) => assert!(
                    spec_says_u16,
                    "info_type {} uses InfoValue::U16 but the ODBC spec defines it wider",
                    entry.info_type
                ),
                InfoValue::U32(_) | InfoValue::Bitmask(_) => assert!(
                    !spec_says_u16,
                    "info_type {} uses InfoValue::U32/Bitmask but the ODBC spec defines it as SQLUSMALLINT",
                    entry.info_type
                ),
                InfoValue::String(_) => {}
            }
            match entry.value {
                InfoValue::String(expected) => {
                    let (rc, value, len) = get_wide_str(h.dbc, entry.info_type);
                    assert_eq!(rc, SQL_SUCCESS, "info_type {}", entry.info_type);
                    assert_eq!(value, expected, "info_type {}", entry.info_type);
                    assert_eq!(
                        len,
                        (expected.encode_utf16().count() * 2) as SqlSmallInt,
                        "info_type {}",
                        entry.info_type
                    );
                }
                InfoValue::U16(expected) => {
                    let (rc, value, len) = get_u16(h.dbc, entry.info_type);
                    assert_eq!(rc, SQL_SUCCESS, "info_type {}", entry.info_type);
                    assert_eq!(value, expected, "info_type {}", entry.info_type);
                    assert_eq!(len, 2, "info_type {}", entry.info_type);
                }
                InfoValue::U32(expected) | InfoValue::Bitmask(expected) => {
                    let (rc, value, len) = get_u32(h.dbc, entry.info_type);
                    assert_eq!(rc, SQL_SUCCESS, "info_type {}", entry.info_type);
                    assert_eq!(value, expected, "info_type {}", entry.info_type);
                    assert_eq!(len, 4, "info_type {}", entry.info_type);
                }
            }
        }

        // Every entry in the independent spec list must exist in
        // STATIC_INFO, or it is silently not exercising anything above.
        for &info_type in SPEC_U16_INFO_TYPES {
            assert!(
                STATIC_INFO.iter().any(|e| e.info_type == info_type),
                "SPEC_U16_INFO_TYPES entry {} has no matching STATIC_INFO entry",
                info_type
            );
        }
    }

    #[test]
    fn compatibility_names_share_canonical_info_types() {
        // SQL_OWNER_USAGE / SQL_QUALIFIER_USAGE are ODBC 1.0 names kept as
        // aliases for the ODBC 2.0+ SQL_SCHEMA_USAGE / SQL_CATALOG_USAGE
        // constants (odbc_types.rs). Pin the alias values themselves, not
        // just the canonical dispatch entries, so an edit that breaks the
        // alias is caught here.
        assert_eq!(odbc::SQL_OWNER_USAGE, odbc::SQL_SCHEMA_USAGE);
        assert_eq!(odbc::SQL_QUALIFIER_USAGE, odbc::SQL_CATALOG_USAGE);

        let schema_entries = STATIC_INFO
            .iter()
            .filter(|entry| entry.info_type == odbc::SQL_SCHEMA_USAGE)
            .count();
        let catalog_entries = STATIC_INFO
            .iter()
            .filter(|entry| entry.info_type == odbc::SQL_CATALOG_USAGE)
            .count();
        assert_eq!(schema_entries, 1);
        assert_eq!(catalog_entries, 1);

        // Exercise the aliases through the real SQLGetInfo dispatch to
        // confirm they resolve to the same reported value as the canonical
        // info types, not just that the constants are numerically equal.
        let h = TestHandles::with_env_dbc();
        let (schema_rc, schema_value, _) = get_u32(h.dbc, odbc::SQL_SCHEMA_USAGE);
        let (owner_rc, owner_value, _) = get_u32(h.dbc, odbc::SQL_OWNER_USAGE);
        assert_eq!(schema_rc, SQL_SUCCESS);
        assert_eq!(owner_rc, SQL_SUCCESS);
        assert_eq!(owner_value, schema_value);

        let (catalog_rc, catalog_value, _) = get_u32(h.dbc, odbc::SQL_CATALOG_USAGE);
        let (qualifier_rc, qualifier_value, _) = get_u32(h.dbc, odbc::SQL_QUALIFIER_USAGE);
        assert_eq!(catalog_rc, SQL_SUCCESS);
        assert_eq!(qualifier_rc, SQL_SUCCESS);
        assert_eq!(qualifier_value, catalog_value);
    }

    #[test]
    fn database_name_reports_current_catalog() {
        let h = TestHandles::with_env_dbc();
        let dbc_ref = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        dbc_ref.inner.lock().unwrap().current_catalog = Some("reporting".to_string());

        let (rc, value, len) = get_wide_str(h.dbc, SQL_DATABASE_NAME);
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(value, "reporting");
        assert_eq!(len, 18);
    }

    #[test]
    fn null_handle_returns_invalid_handle() {
        let (rc, _, _) = get_u16(SQL_NULL_HANDLE, SQL_ACTIVE_STATEMENTS);
        assert_eq!(rc, SQL_INVALID_HANDLE);
    }

    #[test]
    fn u16_info_types_report_expected_values() {
        let h = TestHandles::with_env_dbc();
        for (info_type, expected) in [
            (SQL_MAX_DRIVER_CONNECTIONS, 0u16),
            (SQL_ACTIVE_STATEMENTS, 0),
            (SQL_ODBC_API_CONFORMANCE, SQL_OAC_LEVEL2),
            (SQL_ODBC_SQL_CONFORMANCE, SQL_OSC_CORE),
            (SQL_CURSOR_COMMIT_BEHAVIOR, SQL_CB_CLOSE),
            (SQL_CURSOR_ROLLBACK_BEHAVIOR, SQL_CB_CLOSE),
            (SQL_TXN_CAPABLE, SQL_TC_ALL),
            (SQL_MAX_COLUMN_NAME_LEN, 128),
            (SQL_MAX_SCHEMA_NAME_LEN, 128),
            (SQL_MAX_TABLE_NAME_LEN, 128),
        ] {
            let (rc, val, len) = get_u16(h.dbc, info_type);
            assert_eq!(rc, SQL_SUCCESS, "info_type {info_type}");
            assert_eq!(val, expected, "info_type {info_type}");
            assert_eq!(len, 2, "info_type {info_type}");
        }
    }

    #[test]
    fn u32_info_types_report_expected_values() {
        let h = TestHandles::with_env_dbc();
        for (info_type, expected) in [
            (SQL_GETDATA_EXTENSIONS, SQL_GD_ANY_COLUMN | SQL_GD_ANY_ORDER),
            (SQL_ASYNC_DBC_FUNCTIONS, SQL_ASYNC_DBC_NOT_CAPABLE),
            (SQL_ASYNC_NOTIFICATION, SQL_ASYNC_NOTIFICATION_NOT_CAPABLE),
            (SQL_DEFAULT_TXN_ISOLATION, SQL_TXN_READ_COMMITTED),
            (SQL_TXN_ISOLATION_OPTION, SQL_TXN_ISOLATION_OPTION_SPT),
            (SQL_SQL_CONFORMANCE, SQL_SC_SQL92_ENTRY),
            (
                odbc::SQL_MAX_BINARY_LITERAL_LEN,
                MAX_SQL_BLOCKS * odbc::DEFAULT_PACKET_SIZE,
            ),
            (
                odbc::SQL_MAX_CHAR_LITERAL_LEN,
                MAX_SQL_BLOCKS * odbc::DEFAULT_PACKET_SIZE,
            ),
            (
                SQL_MAX_STATEMENT_LEN,
                MAX_SQL_BLOCKS * odbc::DEFAULT_PACKET_SIZE,
            ),
            // Escape capability masks, measured from msodbcsql18 18.6.2.1.
            (SQL_NUMERIC_FUNCTIONS, SQL_NUMERIC_FUNCTIONS_SUPPORTED),
            (SQL_STRING_FUNCTIONS, SQL_STRING_FUNCTIONS_SUPPORTED),
            (SQL_SYSTEM_FUNCTIONS, SQL_SYSTEM_FUNCTIONS_SUPPORTED),
            (SQL_TIMEDATE_FUNCTIONS, SQL_TIMEDATE_FUNCTIONS_SUPPORTED),
            (SQL_CONVERT_FUNCTIONS, SQL_CONVERT_FUNCTIONS_SUPPORTED),
            (SQL_TIMEDATE_ADD_INTERVALS, SQL_TIMEDATE_INTERVALS_SUPPORTED),
            (
                SQL_TIMEDATE_DIFF_INTERVALS,
                SQL_TIMEDATE_INTERVALS_SUPPORTED,
            ),
            (SQL_OJ_CAPABILITIES, SQL_OJ_CAPABILITIES_SUPPORTED),
        ] {
            let (rc, val, len) = get_u32(h.dbc, info_type);
            assert_eq!(rc, SQL_SUCCESS, "info_type {info_type}");
            assert_eq!(val, expected, "info_type {info_type}");
            assert_eq!(len, 4, "info_type {info_type}");
        }
    }

    #[test]
    fn param_array_info_types_match_odbc_spec_values() {
        // Pinned against the literal `sqlext.h` values (not the driver's own
        // constants) so a transcription slip in either fails this test:
        // `SQL_PARC_NO_BATCH` is 2; `SQL_PAS_BATCH` is 1.
        let h = TestHandles::with_env_dbc();
        for (info_type, expected) in [
            (SQL_PARAM_ARRAY_ROW_COUNTS, 2u32),
            (SQL_PARAM_ARRAY_SELECTS, 1u32),
        ] {
            let (rc, val, len) = get_u32(h.dbc, info_type);
            assert_eq!(rc, SQL_SUCCESS, "info_type {info_type}");
            assert_eq!(val, expected, "info_type {info_type}");
            assert_eq!(len, 4, "info_type {info_type}");
        }
    }

    #[test]
    fn max_statement_len_scales_with_packet_size() {
        assert_eq!(max_statement_len(4096), 512 * 1024);
        assert_eq!(max_statement_len(8192), 1024 * 1024);
        assert_eq!(max_statement_len(32768), 4 * 1024 * 1024);
    }

    #[test]
    fn max_statement_len_saturates_instead_of_overflowing() {
        // `packet_size` is always clamped in practice, but this pure
        // function must still not wrap or panic for out-of-range inputs.
        assert_eq!(max_statement_len(u32::MAX), u32::MAX);
        assert_eq!(max_statement_len(40_000_000), u32::MAX);
    }

    #[test]
    fn sql_text_limits_use_preconnect_packet_size() {
        let h = TestHandles::with_env_dbc();
        let dbc_ref = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        dbc_ref.inner.lock().unwrap().packet_size = 16_384;

        for info_type in [
            odbc::SQL_MAX_BINARY_LITERAL_LEN,
            odbc::SQL_MAX_CHAR_LITERAL_LEN,
            SQL_MAX_STATEMENT_LEN,
        ] {
            let (rc, value, len) = get_u32(h.dbc, info_type);
            assert_eq!(rc, SQL_SUCCESS, "info_type {info_type}");
            assert_eq!(value, MAX_SQL_BLOCKS * 16_384, "info_type {info_type}");
            assert_eq!(len, 4, "info_type {info_type}");
        }
    }

    #[test]
    fn sql_text_limits_report_zero_for_the_zero_packet_size_sentinel() {
        // msodbcsql reads these limits straight out of its stored packet-size
        // dwOption (sqlcinfo.cpp: `MAXSQLBLOCKSSPHINX * dwOptions[SQL_PACKET_SIZE]`),
        // and its `SQL_ATTR_PACKET_SIZE` clamp (sqlcmisc.cpp) only fires when the
        // requested value is truthy, so an explicit 0 is stored verbatim and
        // this multiplication reports 0 for retail too — not the connection's
        // eventual default. A disconnected handle with the zero sentinel must
        // report the same 0, matching that measured behavior exactly.
        let h = TestHandles::with_env_dbc();
        let dbc_ref = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        dbc_ref.inner.lock().unwrap().packet_size = 0;

        for info_type in [
            odbc::SQL_MAX_BINARY_LITERAL_LEN,
            odbc::SQL_MAX_CHAR_LITERAL_LEN,
            SQL_MAX_STATEMENT_LEN,
        ] {
            let (rc, value, len) = get_u32(h.dbc, info_type);
            assert_eq!(rc, SQL_SUCCESS, "info_type {info_type}");
            assert_eq!(value, 0, "info_type {info_type}");
            assert_eq!(len, 4, "info_type {info_type}");
        }
    }

    #[test]
    fn null_string_length_ptr_on_numeric_path_is_ok() {
        let h = TestHandles::with_env_dbc();
        let mut val: u16 = 0xAAAA;
        let rc = unsafe {
            sql_get_info_w(
                h.dbc,
                SQL_ACTIVE_STATEMENTS,
                &mut val as *mut u16 as SqlPointer,
                2,
                ptr::null_mut(),
            )
        };
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(val, 0);
    }

    #[test]
    fn driver_name_writes_wide_string() {
        #[cfg(target_os = "windows")]
        let expected = "mssqlodbc.dll";
        #[cfg(target_os = "macos")]
        let expected = "mssqlodbc.dylib";
        // Mirrors the `_` fallback in build.rs, which emits the `.so` name for
        // every non-Windows, non-macOS target.
        #[cfg(not(any(target_os = "windows", target_os = "macos")))]
        let expected = "mssqlodbc.so";

        assert_eq!(driver_name(), expected);

        let h = TestHandles::with_env_dbc();
        let mut buf = [0u16; 64];
        let mut len: SqlSmallInt = -1;
        let rc = unsafe {
            sql_get_info_w(
                h.dbc,
                SQL_DRIVER_NAME,
                buf.as_mut_ptr() as SqlPointer,
                (buf.len() * std::mem::size_of::<SqlWChar>()) as SqlSmallInt,
                &mut len,
            )
        };
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(len, (expected.encode_utf16().count() * 2) as SqlSmallInt);
        let n = (len as usize) / 2;
        assert_eq!(String::from_utf16_lossy(&buf[..n]), expected);
        // Null-terminated just past the copied text.
        assert_eq!(buf[n], 0);
    }

    #[test]
    fn multiple_active_txn_reports_yes() {
        // One transaction per connection, but several connections may each hold
        // one at once (`sqlcinfo.cpp`).
        let h = TestHandles::with_env_dbc();
        let mut buf = [0u16; 8];
        let mut len: SqlSmallInt = -1;
        let rc = unsafe {
            sql_get_info_w(
                h.dbc,
                SQL_MULTIPLE_ACTIVE_TXN,
                buf.as_mut_ptr() as SqlPointer,
                (buf.len() * std::mem::size_of::<SqlWChar>()) as SqlSmallInt,
                &mut len,
            )
        };
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(len, 2);
        assert_eq!(String::from_utf16_lossy(&buf[..1]), "Y");
    }

    #[test]
    fn null_info_value_ptr_reports_length_only() {
        let h = TestHandles::with_env_dbc();
        let mut len: SqlSmallInt = -1;
        let rc = unsafe { sql_get_info_w(h.dbc, SQL_DBMS_NAME, ptr::null_mut(), 0, &mut len) };
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(
            len,
            ("Microsoft SQL Server".encode_utf16().count() * 2) as SqlSmallInt
        );
    }

    #[test]
    fn wide_string_truncation_returns_info_and_posts_01004() {
        let h = TestHandles::with_env_dbc();
        // "Microsoft SQL Server" needs 40 bytes; give it room for only 3 wchars.
        let mut buf = [0u16; 3];
        let mut len: SqlSmallInt = -1;
        let rc = unsafe {
            sql_get_info_w(
                h.dbc,
                SQL_DBMS_NAME,
                buf.as_mut_ptr() as SqlPointer,
                (buf.len() * std::mem::size_of::<SqlWChar>()) as SqlSmallInt,
                &mut len,
            )
        };
        assert_eq!(rc, SQL_SUCCESS_WITH_INFO);
        // Reported length is the full untruncated byte length.
        assert_eq!(
            len,
            ("Microsoft SQL Server".encode_utf16().count() * 2) as SqlSmallInt
        );
        // Output is null-terminated within the cap: 2 chars + NUL.
        assert_eq!(buf[2], 0);
        assert_eq!(String::from_utf16_lossy(&buf[..2]), "Mi");

        let dbc_ref = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let state = dbc_ref.inner.lock().unwrap();
        assert_eq!(state.diag_records.len(), 1);
        assert_eq!(
            state.diag_records[0].sql_state,
            WARN_STRING_TRUNCATION.state
        );
    }

    #[test]
    fn negative_buffer_length_returns_error() {
        let h = TestHandles::with_env_dbc();
        let mut buf = [0u16; 16];
        let mut len: SqlSmallInt = -1;
        let rc = unsafe {
            sql_get_info_w(
                h.dbc,
                SQL_DRIVER_NAME,
                buf.as_mut_ptr() as SqlPointer,
                -4,
                &mut len,
            )
        };
        assert_eq!(rc, SQL_ERROR);
        let dbc_ref = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let state = dbc_ref.inner.lock().unwrap();
        assert_eq!(state.diag_records.len(), 1);
        assert_eq!(state.diag_records[0].sql_state, *b"HY090");
    }

    #[test]
    fn unsupported_info_type_returns_error() {
        let h = TestHandles::with_env_dbc();
        let (rc, _, _) = get_u16(h.dbc, 65000);
        assert_eq!(rc, SQL_ERROR);

        let dbc_ref = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let state = dbc_ref.inner.lock().unwrap();
        assert_eq!(state.diag_records.len(), 1);
        assert_eq!(state.diag_records[0].sql_state, ERR_INVALID_INFO_TYPE.state);
    }

    // AB#47996: residual ODBC 3.x information types report their msodbcsql
    // values instead of falling through to HY096.
    #[test]
    fn residual_u32_info_types_report_expected_values() {
        let h = TestHandles::with_env_dbc();
        for (info_type, expected) in [
            (odbc::SQL_CONVERT_BIGINT, CVT_NUMBER_SPT),
            (odbc::SQL_CONVERT_BINARY, CVT_BINARY_SPT),
            (odbc::SQL_CONVERT_BIT, CVT_BIT_SPT),
            (odbc::SQL_CONVERT_CHAR, CVT_CHAR_SPT),
            (odbc::SQL_CONVERT_DECIMAL, CVT_NUMBER_SPT),
            (odbc::SQL_CONVERT_FLOAT, CVT_APXNUM_SPT),
            (odbc::SQL_CONVERT_GUID, CVT_GUID_SPT),
            (odbc::SQL_CONVERT_INTEGER, CVT_NUMBER_SPT),
            (odbc::SQL_CONVERT_LONGVARBINARY, CVT_LONGVARBINARY_SPT),
            (odbc::SQL_CONVERT_LONGVARCHAR, CVT_LONGVARCHAR_SPT),
            (odbc::SQL_CONVERT_NUMERIC, CVT_NUMBER_SPT),
            (odbc::SQL_CONVERT_REAL, CVT_APXNUM_SPT),
            (odbc::SQL_CONVERT_SMALLINT, CVT_NUMBER_SPT),
            (odbc::SQL_CONVERT_TIMESTAMP, CVT_TIMESTAMP_SPT),
            (odbc::SQL_CONVERT_TINYINT, CVT_NUMBER_SPT),
            (odbc::SQL_CONVERT_VARBINARY, CVT_BINARY_SPT),
            (odbc::SQL_CONVERT_VARCHAR, CVT_VARCHAR_SPT),
            (odbc::SQL_CONVERT_DATE, 0),
            (odbc::SQL_CONVERT_DOUBLE, 0),
            (odbc::SQL_CONVERT_TIME, 0),
            (odbc::SQL_CONVERT_INTERVAL_DAY_TIME, 0),
            (odbc::SQL_CONVERT_INTERVAL_YEAR_MONTH, 0),
            (odbc::SQL_AGGREGATE_FUNCTIONS, SQL_AF_ALL),
            (odbc::SQL_INDEX_KEYWORDS, SQL_IK_ALL),
            (
                odbc::SQL_INSERT_STATEMENT,
                SQL_IS_INSERT_LITERALS | SQL_IS_INSERT_SEARCHED | SQL_IS_SELECT_INTO,
            ),
            (odbc::SQL_INFO_SCHEMA_VIEWS, SQL_INFO_SCHEMA_VIEWS_MASK),
            // SQLSetPos unimplemented: no positioned operations or lock types.
            (odbc::SQL_LOCK_TYPES, 0),
            (odbc::SQL_POS_OPERATIONS, 0),
            (
                odbc::SQL_CREATE_SCHEMA,
                SQL_CS_CREATE_SCHEMA | SQL_CS_AUTHORIZATION,
            ),
            (odbc::SQL_CREATE_TABLE, SQL_CT_CREATE_TABLE),
            (odbc::SQL_DROP_TABLE, SQL_DT_DROP_TABLE),
            (odbc::SQL_DROP_VIEW, SQL_DV_DROP_VIEW),
            (odbc::SQL_CREATE_VIEW, SQL_CREATE_VIEW_MASK),
            (odbc::SQL_ALTER_DOMAIN, 0),
            (odbc::SQL_DATETIME_LITERALS, 0),
            (odbc::SQL_CREATE_DOMAIN, 0),
            (odbc::SQL_DROP_SCHEMA, 0),
            // Zero-mask capability types msodbcsql still answers (not HY096).
            (odbc::SQL_CREATE_CHARACTER_SET, 0),
            (odbc::SQL_CREATE_COLLATION, 0),
            (odbc::SQL_CREATE_TRANSLATION, 0),
            (odbc::SQL_DROP_ASSERTION, 0),
            (odbc::SQL_DROP_CHARACTER_SET, 0),
            (odbc::SQL_DROP_COLLATION, 0),
            (odbc::SQL_DROP_DOMAIN, 0),
            (odbc::SQL_DROP_TRANSLATION, 0),
            // Wide conversion targets.
            (odbc::SQL_CONVERT_WCHAR, CVT_CHAR_SPT),
            (odbc::SQL_CONVERT_WVARCHAR, CVT_VARCHAR_SPT),
            (odbc::SQL_CONVERT_WLONGVARCHAR, CVT_LONGVARCHAR_SPT),
            // SQL-92 capability masks.
            (odbc::SQL_SQL92_GRANT, SQL_SG_WITH_GRANT_OPTION),
            (odbc::SQL_SQL92_REVOKE, SQL_SR_GRANT_OPTION_FOR),
            (odbc::SQL_SQL92_PREDICATES, SQL_SQL92_PREDICATES_SPT),
            (
                odbc::SQL_SQL92_RELATIONAL_JOIN_OPERATORS,
                SQL_SQL92_RELATIONAL_JOIN_OPERATORS_SPT,
            ),
            (
                odbc::SQL_SQL92_ROW_VALUE_CONSTRUCTOR,
                SQL_SQL92_ROW_VALUE_CONSTRUCTOR_SPT,
            ),
            (
                odbc::SQL_SQL92_STRING_FUNCTIONS,
                SQL_SQL92_STRING_FUNCTIONS_SPT,
            ),
            (
                odbc::SQL_SQL92_VALUE_EXPRESSIONS,
                SQL_SQL92_VALUE_EXPRESSIONS_SPT,
            ),
            (odbc::SQL_SQL92_DATETIME_FUNCTIONS, 0),
            (odbc::SQL_SQL92_FOREIGN_KEY_DELETE_RULE, 0),
            (odbc::SQL_SQL92_FOREIGN_KEY_UPDATE_RULE, 0),
            (odbc::SQL_SQL92_NUMERIC_VALUE_FUNCTIONS, 0),
            (odbc::SQL_MAX_INDEX_SIZE, SQL_SERVER_MAX_INDEX_SIZE),
            (odbc::SQL_MAX_ASYNC_CONCURRENT_STATEMENTS, 0),
            (odbc::SQL_ODBC_INTERFACE_CONFORMANCE, 0),
            (odbc::SQL_STANDARD_CLI_CONFORMANCE, SQL_SCC_ISO92_CLI),
        ] {
            let (rc, val, len) = get_u32(h.dbc, info_type);
            assert_eq!(rc, SQL_SUCCESS, "info_type {info_type}");
            assert_eq!(val, expected, "info_type {info_type}");
            assert_eq!(len, 4, "info_type {info_type}");
        }
    }

    #[test]
    fn residual_u16_info_types_report_expected_values() {
        let h = TestHandles::with_env_dbc();
        for (info_type, expected) in [
            (odbc::SQL_ACTIVE_ENVIRONMENTS, 0u16),
            (odbc::SQL_FILE_USAGE, SQL_FILE_NOT_SUPPORTED),
            (odbc::SQL_CATALOG_LOCATION, SQL_QL_START),
            (odbc::SQL_NON_NULLABLE_COLUMNS, SQL_NNC_NON_NULL),
            (
                odbc::SQL_MAX_CURSOR_NAME_LEN,
                SQL_SERVER_MAX_CURSOR_NAME_LEN,
            ),
            (
                odbc::SQL_MAX_PROCEDURE_NAME_LEN,
                SQL_SERVER_MAX_PROCEDURE_NAME_LEN,
            ),
        ] {
            let (rc, val, len) = get_u16(h.dbc, info_type);
            assert_eq!(rc, SQL_SUCCESS, "info_type {info_type}");
            assert_eq!(val, expected, "info_type {info_type}");
            assert_eq!(len, 2, "info_type {info_type}");
        }
    }

    #[test]
    fn residual_string_info_types_report_expected_values() {
        let h = TestHandles::with_env_dbc();
        for (info_type, expected) in [
            (odbc::SQL_ROW_UPDATES, "N"),
            (odbc::SQL_MAX_ROW_SIZE_INCLUDES_LONG, "N"),
            (odbc::SQL_INTEGRITY, "Y"),
        ] {
            let (rc, value, len) = get_wide_str(h.dbc, info_type);
            assert_eq!(rc, SQL_SUCCESS, "info_type {info_type}");
            assert_eq!(value, expected, "info_type {info_type}");
            assert_eq!(len, 2, "info_type {info_type}");
        }
    }

    // The five `SQL_DRIVER_H*` handle types and `SQL_DRIVER_AWARE_POOLING_SUPPORTED`
    // are answered by the Driver Manager (or, in msodbcsql, `ERROR_FLAG`), so the
    // driver core returns HY096.
    #[test]
    fn handle_and_pooling_info_types_return_hy096() {
        let h = TestHandles::with_env_dbc();
        for info_type in [
            odbc::SQL_DRIVER_HDBC,
            odbc::SQL_DRIVER_HENV,
            odbc::SQL_DRIVER_HSTMT,
            odbc::SQL_DRIVER_HLIB,
            odbc::SQL_DRIVER_HDESC,
            odbc::SQL_DRIVER_AWARE_POOLING_SUPPORTED,
        ] {
            let (rc, _, _) = get_u32(h.dbc, info_type);
            assert_eq!(rc, SQL_ERROR, "info_type {info_type}");
            let dbc_ref = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
            let state = dbc_ref.inner.lock().unwrap();
            assert_eq!(
                state.diag_records[0].sql_state, ERR_INVALID_INFO_TYPE.state,
                "info_type {info_type}"
            );
        }
    }

    #[test]
    fn collation_seq_is_empty_without_a_connection() {
        let h = TestHandles::with_env_dbc();
        let (rc, value, len) = get_wide_str(h.dbc, odbc::SQL_COLLATION_SEQ);
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(value, "");
        assert_eq!(len, 0);
    }

    #[test]
    fn collation_seq_uses_the_cache_while_the_client_is_parked() {
        let h = TestHandles::with_env_dbc();
        let dbc_ref = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        {
            // A data-at-execution sequence has moved the client onto a
            // statement; the DBC stays connected with the collation cached.
            let mut state = dbc_ref.inner.lock().unwrap();
            assert!(state.client.is_none());
            state.last_collation_code_page = Some(1252);
        }
        let (rc, value, len) = get_wide_str(h.dbc, odbc::SQL_COLLATION_SEQ);
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(value, "ISO 8859-1");
        assert_eq!(len, 20);
    }

    #[test]
    fn collation_seq_name_maps_only_the_three_named_code_pages() {
        assert_eq!(collation_seq_name(Some(1252)), Some("ISO 8859-1"));
        assert_eq!(collation_seq_name(Some(850)), Some("Code page 850"));
        assert_eq!(collation_seq_name(Some(437)), Some("Code page 437"));
        assert_eq!(collation_seq_name(Some(1251)), None);
        assert_eq!(collation_seq_name(Some(65001)), None);
        assert_eq!(collation_seq_name(None), None);
    }

    #[test]
    fn char_set_display_name_matches_env_charset_mapping() {
        assert_eq!(char_set_display_name("iso_1"), "ISO 8859-1");
        assert_eq!(char_set_display_name("cp850"), "Code page 850");
        assert_eq!(char_set_display_name("cp1252"), "Code page 1252");
        // Names whose multi-byte characters straddle byte offset 2 (where the
        // old byte slice `&char_set[2..]` cut) must drop two whole characters
        // without panicking on a UTF-8 boundary.
        assert_eq!(char_set_display_name("日本語"), "Code page 語");
        assert_eq!(char_set_display_name("aあx"), "Code page x");
    }

    #[test]
    fn successful_call_clears_previous_diagnostic() {
        let h = TestHandles::with_env_dbc();
        let (rc, _, _) = get_u16(h.dbc, 65000);
        assert_eq!(rc, SQL_ERROR);

        let (rc, _, _) = get_u16(h.dbc, SQL_ACTIVE_STATEMENTS);
        assert_eq!(rc, SQL_SUCCESS);

        let dbc_ref = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        assert!(dbc_ref.inner.lock().unwrap().diag_records.is_empty());
    }

    #[test]
    fn string_info_types_report_expected_values() {
        let h = TestHandles::with_env_dbc();
        for (info_type, expected) in STRING_CASES {
            let (rc, value, len) = get_wide_str(h.dbc, *info_type);
            assert_eq!(rc, SQL_SUCCESS, "info_type {info_type}");
            assert_eq!(&value, expected, "info_type {info_type}");
            assert_eq!(
                len,
                (expected.encode_utf16().count() * 2) as SqlSmallInt,
                "info_type {info_type}"
            );
        }
    }

    #[test]
    fn string_info_types_report_length_with_null_buffer() {
        let h = TestHandles::with_env_dbc();
        for (info_type, expected) in STRING_CASES {
            let mut len: SqlSmallInt = -1;
            let rc = unsafe { sql_get_info_w(h.dbc, *info_type, ptr::null_mut(), 0, &mut len) };
            assert_eq!(rc, SQL_SUCCESS, "info_type {info_type}");
            assert_eq!(
                len,
                (expected.encode_utf16().count() * 2) as SqlSmallInt,
                "info_type {info_type}"
            );
        }
    }

    #[test]
    fn string_info_types_truncate_with_01004() {
        let h = TestHandles::with_env_dbc();
        for (info_type, expected) in STRING_CASES {
            // A one-character value already fills a 2-wchar buffer, so only the
            // longer values can demonstrate truncation.
            if expected.encode_utf16().count() < 2 {
                continue;
            }
            let mut buf = [0u16; 2];
            let mut len: SqlSmallInt = -1;
            let rc = unsafe {
                sql_get_info_w(
                    h.dbc,
                    *info_type,
                    buf.as_mut_ptr() as SqlPointer,
                    (buf.len() * std::mem::size_of::<SqlWChar>()) as SqlSmallInt,
                    &mut len,
                )
            };
            assert_eq!(rc, SQL_SUCCESS_WITH_INFO, "info_type {info_type}");
            // The full untruncated length is still reported.
            assert_eq!(
                len,
                (expected.encode_utf16().count() * 2) as SqlSmallInt,
                "info_type {info_type}"
            );
            assert_eq!(buf[1], 0, "info_type {info_type}: missing NUL");
            assert_eq!(
                String::from_utf16_lossy(&buf[..1]),
                expected.chars().next().unwrap().to_string(),
                "info_type {info_type}"
            );

            let dbc_ref = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
            let state = dbc_ref.inner.lock().unwrap();
            assert_eq!(
                state.diag_records.last().map(|d| d.sql_state),
                Some(WARN_STRING_TRUNCATION.state),
                "info_type {info_type}"
            );
        }
    }

    #[test]
    fn identity_info_types_are_empty_before_connect() {
        let h = TestHandles::with_env_dbc();
        for info_type in [SQL_DATA_SOURCE_NAME, SQL_SERVER_NAME, SQL_USER_NAME] {
            let (rc, value, len) = get_wide_str(h.dbc, info_type);
            assert_eq!(rc, SQL_SUCCESS, "info_type {info_type}");
            assert_eq!(value, "", "info_type {info_type}");
            assert_eq!(len, 0, "info_type {info_type}");
        }
    }

    #[test]
    fn identity_info_types_report_connected_session() {
        let h = TestHandles::with_env_dbc();
        {
            let dbc_ref = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
            let mut state = dbc_ref.inner.lock().unwrap();
            state.identity = crate::handles::dbc::ConnectionIdentity {
                data_source_name: "ReportingDsn".to_string(),
                server_name: "SQLPROD01\\INST".to_string(),
            };
        }

        for (info_type, expected) in [
            (SQL_DATA_SOURCE_NAME, "ReportingDsn"),
            (SQL_SERVER_NAME, "SQLPROD01\\INST"),
        ] {
            let (rc, value, len) = get_wide_str(h.dbc, info_type);
            assert_eq!(rc, SQL_SUCCESS, "info_type {info_type}");
            assert_eq!(value, expected, "info_type {info_type}");
            assert_eq!(
                len,
                (expected.encode_utf16().count() * 2) as SqlSmallInt,
                "info_type {info_type}"
            );
        }
    }

    /// Shared setup for the `SQL_USER_NAME` cases: a mock server answering the
    /// lookup with `first`, and a connected DBC.
    fn user_name_fixture(
        first: mssql_mock_tds::QueryResponse,
    ) -> (TestHandles, crate::test_support::MockServer) {
        let h = TestHandles::with_env_dbc();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let server = crate::test_support::connect_mock_server(dbc, DATABASE_USER_NAME_QUERY, first);
        (h, server)
    }

    fn user_name_row(value: &str) -> mssql_mock_tds::QueryResponse {
        use mssql_mock_tds::{ColumnDefinition, ColumnValue, QueryResponse, Row, SqlDataType};
        QueryResponse::new(
            vec![ColumnDefinition::new("", SqlDataType::NVarChar)],
            vec![Row::new(vec![ColumnValue::NVarChar(value.to_string())])],
        )
    }

    fn diag_states(dbc: SqlHandle) -> Vec<String> {
        let dbc_ref = unsafe { handle_from_raw::<DbcHandle>(dbc) };
        let state = dbc_ref.inner.lock().unwrap();
        state
            .diag_records
            .iter()
            .map(|d| String::from_utf8_lossy(&d.sql_state).into_owned())
            .collect()
    }

    fn set_catalog(dbc: SqlHandle, name: &str) -> SqlReturn {
        use crate::api::odbc_types::{SQL_ATTR_CURRENT_CATALOG, SQL_NTS, SqlInteger};
        use crate::api::set_connect_attr::sql_set_connect_attr_w;
        let wide: Vec<u16> = format!("{name}\0").encode_utf16().collect();
        unsafe {
            sql_set_connect_attr_w(
                dbc,
                SQL_ATTR_CURRENT_CATALOG,
                wide.as_ptr() as SqlPointer,
                SqlInteger::from(SQL_NTS),
            )
        }
    }

    /// The lookup is lazy — nothing is cached until the first ask — and the
    /// answer is then reused, matching msodbcsql's `CONN_ST_REFRESH_UDT` model
    /// rather than paying a round trip at connect.
    #[test]
    fn user_name_is_queried_lazily_then_served_from_the_cache() {
        let (h, server) = user_name_fixture(user_name_row("dbo"));
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };

        assert!(
            dbc.inner.lock().unwrap().database_user_name.is_none(),
            "connecting must not pay the USER_NAME() round trip"
        );

        let (rc, value, len) = get_wide_str(h.dbc, SQL_USER_NAME);
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(value, "dbo");
        assert_eq!(len, 6, "byte count, not character count");
        assert!(diag_states(h.dbc).is_empty());

        // Re-registering proves the second call never reaches the server.
        server.register_query(DATABASE_USER_NAME_QUERY, user_name_row("report_reader"));
        assert_eq!(get_wide_str(h.dbc, SQL_USER_NAME).1, "dbo");
    }

    /// The value is database-scoped, so `SQL_ATTR_CURRENT_CATALOG` must make the
    /// next read go back to the server.
    #[test]
    fn user_name_is_refreshed_after_a_catalog_change() {
        use mssql_mock_tds::QueryResponse;

        let (h, server) = user_name_fixture(user_name_row("dbo"));
        assert_eq!(get_wide_str(h.dbc, SQL_USER_NAME).1, "dbo");

        server.register_query(DATABASE_USER_NAME_QUERY, user_name_row("report_reader"));
        server.register_query(
            "USE [reporting]",
            QueryResponse::new(Vec::new(), Vec::new()),
        );
        assert_eq!(set_catalog(h.dbc, "reporting"), SQL_SUCCESS);

        assert_eq!(
            get_wide_str(h.dbc, SQL_USER_NAME),
            (SQL_SUCCESS, "report_reader".to_string(), 26)
        );
    }

    /// A `USE` the driver did not issue still reaches the client as an
    /// ENVCHANGE, so the catalog key — not just the attribute path — has to
    /// invalidate the entry. Driven directly here because the mock server does
    /// not emit ENVCHANGE for a registered batch.
    #[test]
    fn user_name_is_refreshed_when_the_client_reports_a_different_catalog() {
        let (h, server) = user_name_fixture(user_name_row("dbo"));
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        assert_eq!(get_wide_str(h.dbc, SQL_USER_NAME).1, "dbo");

        {
            let mut state = dbc.inner.lock().unwrap();
            let cached = state.database_user_name.as_mut().unwrap();
            cached.catalog = "some_other_database".to_string();
        }
        server.register_query(DATABASE_USER_NAME_QUERY, user_name_row("guest"));

        assert_eq!(get_wide_str(h.dbc, SQL_USER_NAME).1, "guest");
    }

    /// Case-insensitive, so a server that reports `MASTER` where the lookup ran
    /// in `master` does not force a needless round trip — the same comparison
    /// `SQL_ATTR_CURRENT_CATALOG` uses to decide a `USE` is redundant.
    #[test]
    fn user_name_cache_matches_the_catalog_case_insensitively() {
        let (h, server) = user_name_fixture(user_name_row("dbo"));
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        assert_eq!(get_wide_str(h.dbc, SQL_USER_NAME).1, "dbo");

        {
            let mut state = dbc.inner.lock().unwrap();
            let cached = state.database_user_name.as_mut().unwrap();
            cached.catalog = cached.catalog.to_uppercase();
        }
        server.register_query(DATABASE_USER_NAME_QUERY, user_name_row("changed"));

        assert_eq!(get_wide_str(h.dbc, SQL_USER_NAME).1, "dbo");
    }

    /// msodbcsql spawns a second connection when the first is busy and, if that
    /// fails, reports the cached value — `RefreshShilohUDTCache` cannot fail the
    /// caller. This driver has no spawn facility, so the cached value is the
    /// whole answer; what must not happen is an error or a diagnostic, because
    /// `SQLGetInfo` has to stay answerable while a cursor is open.
    #[test]
    fn user_name_reports_the_cached_value_while_the_connection_is_busy() {
        let (mut h, server) = user_name_fixture(user_name_row("dbo"));
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        assert_eq!(get_wide_str(h.dbc, SQL_USER_NAME).1, "dbo");

        let stmt = h.alloc_extra_stmt();
        dbc.inner.lock().unwrap().active_stmt = Some(stmt);
        server.register_query(DATABASE_USER_NAME_QUERY, user_name_row("never_read"));

        let (rc, value, _) = get_wide_str(h.dbc, SQL_USER_NAME);
        assert_eq!(
            rc, SQL_SUCCESS,
            "a busy connection must not fail SQLGetInfo"
        );
        assert_eq!(value, "dbo");
        assert!(
            diag_states(h.dbc).is_empty(),
            "the refusal to refresh is internal and must post nothing"
        );

        dbc.inner.lock().unwrap().active_stmt = None;
    }

    /// The same busy path before anything was cached: an empty string, still
    /// `SQL_SUCCESS`. msodbcsql reports its zero-initialized buffer here.
    #[test]
    fn user_name_is_empty_when_busy_before_the_first_lookup() {
        let (mut h, _server) = user_name_fixture(user_name_row("dbo"));
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };

        let stmt = h.alloc_extra_stmt();
        dbc.inner.lock().unwrap().active_stmt = Some(stmt);

        assert_eq!(
            get_wide_str(h.dbc, SQL_USER_NAME),
            (SQL_SUCCESS, String::new(), 0)
        );
        assert!(diag_states(h.dbc).is_empty());

        dbc.inner.lock().unwrap().active_stmt = None;
    }

    /// A server-side failure leaves the previous answer standing and posts
    /// nothing — msodbcsql's `ExecImmediate` failure path skips the fetch, keeps
    /// `DBUserName`, and discards the records with its driver statement. The
    /// connection must also survive, so the next lookup can succeed.
    #[test]
    fn user_name_survives_a_failed_lookup_without_diagnostics() {
        use mssql_mock_tds::{QueryResponse, TerminalError};

        let (h, server) = user_name_fixture(user_name_row("dbo"));
        assert_eq!(get_wide_str(h.dbc, SQL_USER_NAME).1, "dbo");

        // Force a refresh, then make that refresh fail.
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        dbc.inner
            .lock()
            .unwrap()
            .database_user_name
            .as_mut()
            .unwrap()
            .catalog = "elsewhere".to_string();
        server.register_query(
            DATABASE_USER_NAME_QUERY,
            QueryResponse::error_only(TerminalError::new(
                229,
                14,
                "The SELECT permission was denied on the object 'USER_NAME'",
            )),
        );

        let (rc, value, _) = get_wide_str(h.dbc, SQL_USER_NAME);
        assert_eq!(
            rc, SQL_SUCCESS,
            "a failed internal lookup is not the caller's error"
        );
        assert_eq!(value, "dbo", "the previous answer stands");
        assert!(diag_states(h.dbc).is_empty());

        // The client went back idle, so a later refresh still works.
        server.register_query(DATABASE_USER_NAME_QUERY, user_name_row("recovered"));
        assert_eq!(get_wide_str(h.dbc, SQL_USER_NAME).1, "recovered");
    }

    /// `USER_NAME()` is NULL for a login with no principal in the current
    /// database. msodbcsql's `SQLGetData` leaves its buffer untouched for a
    /// NULL, so the previous answer stands rather than being cleared — and
    /// because the exec itself succeeded, it stops asking as well.
    #[test]
    fn user_name_keeps_the_previous_answer_for_a_null_result() {
        use mssql_mock_tds::{ColumnDefinition, ColumnValue, QueryResponse, Row, SqlDataType};

        let (h, server) = user_name_fixture(user_name_row("dbo"));
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        assert_eq!(get_wide_str(h.dbc, SQL_USER_NAME).1, "dbo");

        dbc.inner
            .lock()
            .unwrap()
            .database_user_name
            .as_mut()
            .unwrap()
            .catalog = "elsewhere".to_string();
        server.register_query(
            DATABASE_USER_NAME_QUERY,
            QueryResponse::new(
                vec![ColumnDefinition::new("", SqlDataType::Int)],
                vec![Row::new(vec![ColumnValue::Null])],
            ),
        );

        let (rc, value, _) = get_wide_str(h.dbc, SQL_USER_NAME);
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(value, "dbo");
        assert!(diag_states(h.dbc).is_empty());

        // Re-keyed to the database the NULL came from, so the next read is
        // served from the cache instead of repeating the round trip.
        server.register_query(DATABASE_USER_NAME_QUERY, user_name_row("never_read"));
        assert_eq!(get_wide_str(h.dbc, SQL_USER_NAME).1, "dbo");
    }

    /// An empty result set is the other "nothing to report" shape. The query
    /// still *ran*, so — like msodbcsql, which clears `CONN_ST_REFRESH_UDT` on
    /// exec success before it fetches — the outcome is cached and the round
    /// trip is not repeated on every later call.
    #[test]
    fn user_name_tolerates_a_lookup_that_returns_no_row() {
        use mssql_mock_tds::{ColumnDefinition, QueryResponse, SqlDataType};

        let (h, server) = user_name_fixture(QueryResponse::new(
            vec![ColumnDefinition::new("", SqlDataType::NVarChar)],
            Vec::new(),
        ));

        assert_eq!(
            get_wide_str(h.dbc, SQL_USER_NAME),
            (SQL_SUCCESS, String::new(), 0)
        );
        assert!(diag_states(h.dbc).is_empty());

        // Registering a real answer the second call must not see proves the
        // lookup stopped asking rather than re-querying forever.
        server.register_query(DATABASE_USER_NAME_QUERY, user_name_row("never_read"));
        assert_eq!(
            get_wide_str(h.dbc, SQL_USER_NAME),
            (SQL_SUCCESS, String::new(), 0)
        );
    }

    /// `SQL_ATTR_CONNECTION_TIMEOUT` bounds the internal query, as it does in
    /// msodbcsql (`GetNetIOTimeOut` → `ExecImmediate`). Expiry must still not
    /// fail the caller: the deadline ends the *lookup*, not the `SQLGetInfo`.
    #[test]
    fn user_name_lookup_is_bounded_by_the_connection_timeout() {
        use std::time::{Duration, Instant};

        const RESPONSE_DELAY: Duration = Duration::from_secs(8);
        const TIMEOUT_SECS: u32 = 1;
        // Comfortably above the timeout plus RTT, comfortably below the delay —
        // the gap is what proves the deadline, not the server, ended the wait.
        const BOUND: Duration = Duration::from_secs(5);

        let (h, _server) = user_name_fixture(user_name_row("dbo").with_delay(RESPONSE_DELAY));
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        dbc.inner.lock().unwrap().connection_timeout = TIMEOUT_SECS;

        let started = Instant::now();
        let (rc, value, _) = get_wide_str(h.dbc, SQL_USER_NAME);
        let elapsed = started.elapsed();

        assert!(
            elapsed < BOUND,
            "SQLGetInfo took {elapsed:?} — a {TIMEOUT_SECS}s SQL_ATTR_CONNECTION_TIMEOUT must \
             bound the lookup well below the server's {RESPONSE_DELAY:?} delay"
        );
        assert_eq!(
            rc, SQL_SUCCESS,
            "a lookup deadline is not the caller's error"
        );
        assert_eq!(value, "", "nothing was learned, so nothing is reported");
        assert!(diag_states(h.dbc).is_empty());
    }

    /// An invalid buffer length must be rejected *before* the driver issues a
    /// query on the caller's behalf. Otherwise `SQLGetInfoW(..., -1, ...)` pays
    /// — and caches — a round trip only to return `HY090`, and with no
    /// connection timeout set it could block indefinitely first.
    #[test]
    fn user_name_rejects_a_negative_buffer_length_without_querying() {
        let (h, server) = user_name_fixture(user_name_row("dbo"));
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };

        let mut buf = [0u16; 8];
        let mut len: SqlSmallInt = -1;
        let rc = unsafe {
            sql_get_info_w(
                h.dbc,
                SQL_USER_NAME,
                buf.as_mut_ptr() as SqlPointer,
                -1,
                &mut len,
            )
        };

        assert_eq!(rc, SQL_ERROR);
        assert_eq!(diag_states(h.dbc), vec!["HY090"]);
        assert!(
            dbc.inner.lock().unwrap().database_user_name.is_none(),
            "no query may be issued, so nothing may be cached"
        );

        // Registering a different answer the next call must not see would be
        // ambiguous; instead prove the lookup still works afterwards.
        server.register_query(DATABASE_USER_NAME_QUERY, user_name_row("dbo"));
        assert_eq!(get_wide_str(h.dbc, SQL_USER_NAME).1, "dbo");
    }

    /// The previous call's diagnostics must be cleared at entry, not after the
    /// lookup returns — otherwise they stay visible to `SQLGetDiagRec` for the
    /// whole round trip, and survive entirely if the lookup never completes.
    #[test]
    fn user_name_clears_diagnostics_before_the_lookup_runs() {
        use std::time::Duration;

        let (h, _server) =
            user_name_fixture(user_name_row("dbo").with_delay(Duration::from_secs(4)));
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        dbc.inner.lock().unwrap().connection_timeout = 1;

        // Leave a record behind from an earlier call.
        let mut small = [0u16; 2];
        let mut len: SqlSmallInt = -1;
        assert_eq!(
            unsafe {
                sql_get_info_w(
                    h.dbc,
                    SQL_SERVER_NAME,
                    small.as_mut_ptr() as SqlPointer,
                    -1,
                    &mut len,
                )
            },
            SQL_ERROR
        );
        assert_eq!(diag_states(h.dbc), vec!["HY090"]);

        // Observe the records from another thread while the lookup is blocked
        // on the server: they must already be gone.
        let probe = std::thread::spawn({
            let dbc_ptr = h.dbc as usize;
            move || {
                std::thread::sleep(Duration::from_millis(400));
                let dbc = unsafe { handle_from_raw::<DbcHandle>(dbc_ptr as SqlHandle) };
                let state = dbc.inner.lock().unwrap();
                state.diag_records.len()
            }
        });

        let (rc, _, _) = get_wide_str(h.dbc, SQL_USER_NAME);
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(
            probe.join().unwrap(),
            0,
            "the previous call's diagnostics must be cleared before the lookup, \
             not after it returns"
        );
    }

    /// A failure *after* the server accepted the batch is on msodbcsql's
    /// stop-asking side of the boundary: `CONN_ST_REFRESH_UDT` is already clear
    /// by then, so the previous answer stands and is not re-queried. Only a
    /// failure to execute leaves the refresh outstanding.
    #[test]
    fn user_name_caches_the_fallback_when_the_drain_fails_after_execution() {
        use mssql_mock_tds::{ColumnDefinition, QueryResponse, Row, SqlDataType};

        let (h, server) = user_name_fixture(user_name_row("dbo"));
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        assert_eq!(get_wide_str(h.dbc, SQL_USER_NAME).1, "dbo");

        // Force a refresh, and make that refresh fail mid-drain: the response
        // announces a column it never sends a complete row for.
        dbc.inner
            .lock()
            .unwrap()
            .database_user_name
            .as_mut()
            .unwrap()
            .catalog = "elsewhere".to_string();
        server.register_query(
            DATABASE_USER_NAME_QUERY,
            QueryResponse::new(
                vec![
                    ColumnDefinition::new("", SqlDataType::NVarChar),
                    ColumnDefinition::new("", SqlDataType::Int),
                ],
                vec![Row::new(vec![mssql_mock_tds::ColumnValue::NVarChar(
                    "partial".to_string(),
                )])],
            ),
        );

        let (rc, value, _) = get_wide_str(h.dbc, SQL_USER_NAME);
        assert_eq!(rc, SQL_SUCCESS);
        assert!(diag_states(h.dbc).is_empty());

        // Whatever it reported, it must not go back to the server for it.
        server.register_query(DATABASE_USER_NAME_QUERY, user_name_row("never_read"));
        assert_eq!(
            get_wide_str(h.dbc, SQL_USER_NAME).1,
            value,
            "a post-execution failure must stop asking, like msodbcsql's cleared flag"
        );
    }

    /// The lookup releases the DBC lock for its round trip. If the session is
    /// replaced in that window, the client in hand belongs to a closed session:
    /// storing it would overwrite the live one and attach the old principal to
    /// it.
    #[test]
    fn user_name_discards_its_answer_when_the_session_is_replaced() {
        let (h, _server) = user_name_fixture(user_name_row("dbo"));
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };

        // Stand in for "a disconnect and a fresh connect completed while the
        // lookup was in flight" by publishing a newer generation.
        let (client, generation) = {
            let mut state = dbc.inner.lock().unwrap();
            let client = state.client.take().expect("fixture installs a client");
            let generation = state.session_generation;
            state.session_generation = generation.wrapping_add(1);
            (client, generation)
        };

        let reported = super::publish_lookup(
            dbc,
            client,
            generation,
            "master".to_string(),
            Some("dbo".to_string()),
            "stale".to_string(),
        );

        assert_eq!(reported, "", "nothing is known about the new session");
        let state = dbc.inner.lock().unwrap();
        assert!(
            state.client.is_none(),
            "the stale client must not be installed over the new session"
        );
        assert!(
            state.database_user_name.is_none(),
            "the old session's principal must not be cached against the new one"
        );
    }
    /// The internal query's INFO messages must not reach the application. They
    /// belong to no application statement, so a leak would turn an unrelated
    /// `SQLExecDirect` into `SQL_SUCCESS_WITH_INFO` carrying a message the
    /// caller never provoked.
    ///
    /// The guarantee comes from `TdsClient::begin_command`, which clears
    /// `info_messages` at the top of every command — not from anything this
    /// module does. That is precisely why it is worth pinning here: the
    /// property this driver depends on lives one layer down, where a change
    /// would not obviously implicate `SQLGetInfo`.
    #[test]
    fn user_name_lookup_does_not_leak_info_messages_to_the_next_statement() {
        use crate::api::exec_direct::sql_exec_direct_w;
        use crate::api::odbc_types::SQL_NTS;
        use mssql_mock_tds::InfoMessage;

        let h = TestHandles::with_env_dbc_stmt();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        let _server = crate::test_support::connect_mock_server(
            dbc,
            DATABASE_USER_NAME_QUERY,
            user_name_row("dbo").with_info_tokens(vec![InfoMessage::new(
                5701,
                0,
                "Changed database context to 'master'.",
            )]),
        );

        assert_eq!(get_wide_str(h.dbc, SQL_USER_NAME).1, "dbo");
        assert!(
            diag_states(h.dbc).is_empty(),
            "the internal query's INFO must not reach the DBC either"
        );

        // "SELECT 1" is a default registration on the mock server and carries
        // no INFO of its own, so anything reported here leaked from the lookup.
        let sql: Vec<u16> = "SELECT 1\0".encode_utf16().collect();
        let rc = unsafe { sql_exec_direct_w(h.stmt, sql.as_ptr(), SQL_NTS) };

        let stmt = unsafe { handle_from_raw::<crate::handles::StmtHandle>(h.stmt) };
        let leaked: Vec<String> = stmt
            .inner
            .lock()
            .unwrap()
            .diag_records
            .iter()
            .map(|d| format!("{} {}", String::from_utf8_lossy(&d.sql_state), d.message))
            .collect();
        assert!(
            leaked.is_empty(),
            "the lookup's INFO surfaced on an unrelated statement: {leaked:?}"
        );
        assert_eq!(
            rc, SQL_SUCCESS,
            "a leaked INFO would also downgrade this to SQL_SUCCESS_WITH_INFO"
        );
    }

    /// The lookup must leave the connection idle and usable: it claims the
    /// client only for the round trip and returns it, never keeping
    /// `active_stmt`.
    #[test]
    fn user_name_lookup_leaves_the_connection_idle() {
        let (h, _server) = user_name_fixture(user_name_row("dbo"));
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };

        assert_eq!(get_wide_str(h.dbc, SQL_USER_NAME).1, "dbo");

        let state = dbc.inner.lock().unwrap();
        assert!(state.client.is_some(), "the client must go back to the DBC");
        assert!(
            state.active_stmt.is_none(),
            "no cursor claim may be left behind"
        );
        assert_eq!(state.connection_state, ConnectionState::Connected);
        assert!(
            !state.local_tran_started,
            "the internal lookup must not open a user transaction"
        );
        // `local_tran_started` is the driver's own bookkeeping, which the
        // internal lookup bypasses entirely — so on its own it would stay false
        // even if the batch had left a transaction open on the server. Ask the
        // client what actually happened on the wire.
        assert!(
            !state
                .client
                .as_ref()
                .expect("client is present")
                .has_active_transaction(),
            "the internal lookup must not leave the session in a server transaction"
        );
    }

    /// A name longer than the caller's buffer truncates with `01004` and
    /// reports the full length, like every other string information type.
    #[test]
    fn user_name_truncates_with_01004() {
        let (h, _server) = user_name_fixture(user_name_row("reporting_reader"));

        let mut buf = [0u16; 4];
        let mut len: SqlSmallInt = -1;
        let rc = unsafe {
            sql_get_info_w(
                h.dbc,
                SQL_USER_NAME,
                buf.as_mut_ptr() as SqlPointer,
                (buf.len() * std::mem::size_of::<SqlWChar>()) as SqlSmallInt,
                &mut len,
            )
        };

        assert_eq!(rc, SQL_SUCCESS_WITH_INFO);
        assert_eq!(
            len, 32,
            "the full untruncated byte length is still reported"
        );
        assert_eq!(String::from_utf16_lossy(&buf[..3]), "rep");
        assert_eq!(buf[3], 0, "missing NUL");
        assert_eq!(diag_states(h.dbc), vec!["01004"]);
    }

    /// A null buffer is a length probe, and must not re-run the lookup.
    #[test]
    fn user_name_reports_its_length_with_a_null_buffer() {
        let (h, server) = user_name_fixture(user_name_row("dbo"));

        let mut len: SqlSmallInt = -1;
        let rc = unsafe { sql_get_info_w(h.dbc, SQL_USER_NAME, ptr::null_mut(), 0, &mut len) };
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(len, 6);

        server.register_query(DATABASE_USER_NAME_QUERY, user_name_row("changed"));
        assert_eq!(get_wide_str(h.dbc, SQL_USER_NAME).1, "dbo");
    }

    /// A non-ASCII principal survives the UTF-16 round trip with a byte length
    /// measured in code units, not characters.
    #[test]
    fn user_name_reports_a_non_ascii_principal() {
        let (h, _server) = user_name_fixture(user_name_row("análisis"));

        let (rc, value, len) = get_wide_str(h.dbc, SQL_USER_NAME);
        assert_eq!(rc, SQL_SUCCESS);
        assert_eq!(value, "análisis");
        assert_eq!(len, 16);
    }

    /// Disconnecting ends the session the name belonged to, so the entry must
    /// not survive into the handle's next connection.
    #[test]
    fn user_name_cache_is_dropped_on_disconnect() {
        let (h, _server) = user_name_fixture(user_name_row("dbo"));
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        assert_eq!(get_wide_str(h.dbc, SQL_USER_NAME).1, "dbo");

        assert_eq!(
            unsafe { crate::api::disconnect::sql_disconnect(h.dbc) },
            SQL_SUCCESS
        );

        assert!(dbc.inner.lock().unwrap().database_user_name.is_none());
        assert_eq!(
            get_wide_str(h.dbc, SQL_USER_NAME),
            (SQL_SUCCESS, String::new(), 0),
            "a disconnected handle reports no database user"
        );
    }

    /// The lookup must not resurrect diagnostics the call is supposed to clear.
    #[test]
    fn user_name_clears_the_previous_calls_diagnostic() {
        let (h, _server) = user_name_fixture(user_name_row("dbo"));

        let mut small = [0u16; 2];
        let mut len: SqlSmallInt = -1;
        assert_eq!(
            unsafe {
                sql_get_info_w(
                    h.dbc,
                    SQL_USER_NAME,
                    small.as_mut_ptr() as SqlPointer,
                    (small.len() * std::mem::size_of::<SqlWChar>()) as SqlSmallInt,
                    &mut len,
                )
            },
            SQL_SUCCESS_WITH_INFO
        );
        assert_eq!(diag_states(h.dbc), vec!["01004"]);

        assert_eq!(get_wide_str(h.dbc, SQL_USER_NAME).0, SQL_SUCCESS);
        assert!(diag_states(h.dbc).is_empty());
    }

    #[test]
    fn identity_info_types_truncate_with_01004() {
        let h = TestHandles::with_env_dbc();
        {
            let dbc_ref = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
            let mut state = dbc_ref.inner.lock().unwrap();
            state.identity.server_name = "SQLPROD01".to_string();
        }

        let mut buf = [0u16; 4];
        let mut len: SqlSmallInt = -1;
        let rc = unsafe {
            sql_get_info_w(
                h.dbc,
                SQL_SERVER_NAME,
                buf.as_mut_ptr() as SqlPointer,
                (buf.len() * std::mem::size_of::<SqlWChar>()) as SqlSmallInt,
                &mut len,
            )
        };
        assert_eq!(rc, SQL_SUCCESS_WITH_INFO);
        assert_eq!(len, 18);
        assert_eq!(String::from_utf16_lossy(&buf[..3]), "SQL");
        assert_eq!(buf[3], 0);
    }

    #[test]
    fn special_characters_match_msodbcsql() {
        // `#` and `$`, then every Latin-1 letter. U+00D7 and U+00F7 are the
        // multiplication and division signs, not letters, so they are excluded.
        let mut expected = vec!['#', '$'];
        expected.extend(
            (0xC0u32..=0xFFu32)
                .filter(|c| *c != 0xD7 && *c != 0xF7)
                .map(|c| char::from_u32(c).expect("Latin-1 range is valid Unicode")),
        );
        assert_eq!(
            SQL_SERVER_SPECIAL_CHARACTERS.chars().collect::<Vec<_>>(),
            expected
        );
        // msodbcsql18 reports 64 characters (128 bytes as UTF-16).
        assert_eq!(SQL_SERVER_SPECIAL_CHARACTERS.chars().count(), 64);
    }

    #[test]
    fn keywords_are_a_bare_comma_separated_upper_case_list() {
        // msodbcsql18 reports 69 words in 1090 bytes of UTF-16, with no spaces
        // around the separators; an application splits on ',' verbatim.
        assert_eq!(SQL_SERVER_KEYWORDS.encode_utf16().count() * 2, 1090);
        let words: Vec<&str> = SQL_SERVER_KEYWORDS.split(',').collect();
        assert_eq!(words.len(), 69);
        for word in &words {
            assert!(!word.is_empty(), "empty keyword");
            assert_eq!(*word, word.to_ascii_uppercase(), "keyword not upper case");
            assert!(
                !word.contains(char::is_whitespace),
                "keyword {word} contains whitespace"
            );
        }
    }
}
