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
use crate::api::util::{copy_with_nul, write_if_some};
use crate::error::free_errors;
use crate::handles::{DbcHandle, HandleType, handle_from_raw};

/// `sysname`, the type of every identifier column in the catalog views, which
/// bounds `SQL_MAX_COLUMN_NAME_LEN`, `SQL_MAX_SCHEMA_NAME_LEN`, and
/// `SQL_MAX_TABLE_NAME_LEN` alike.
const MAX_IDENTIFIER_LEN: u16 = 128;

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
    let Ok(mut state) = dbc.inner.lock() else {
        error!("SQLGetInfoW: dbc mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut state);

    unsafe { write_if_some(string_length_ptr, 0) };

    match info_type {
        SQL_DATA_SOURCE_NAME | SQL_SERVER_NAME | SQL_USER_NAME => {
            let value = if info_type == SQL_DATA_SOURCE_NAME {
                state.identity.data_source_name.clone()
            } else if info_type == SQL_SERVER_NAME {
                state.identity.server_name.clone()
            } else {
                state.identity.user_name.clone()
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
    ];

    #[test]
    fn static_info_types_are_unique_and_report_typed_values() {
        let h = TestHandles::with_env_dbc();
        let mut seen = HashSet::new();

        assert_eq!(STATIC_INFO.len(), 50);
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
                user_name: "reporting_app".to_string(),
            };
        }

        for (info_type, expected) in [
            (SQL_DATA_SOURCE_NAME, "ReportingDsn"),
            (SQL_SERVER_NAME, "SQLPROD01\\INST"),
            (SQL_USER_NAME, "reporting_app"),
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
