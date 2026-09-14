// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLGetInfoW.

use tracing::{debug, error};

use crate::api::odbc_types::{
    SQL_ACCESSIBLE_PROCEDURES, SQL_ACCESSIBLE_TABLES, SQL_ACTIVE_STATEMENTS,
    SQL_ASYNC_DBC_FUNCTIONS, SQL_ASYNC_DBC_NOT_CAPABLE, SQL_ASYNC_NOTIFICATION,
    SQL_ASYNC_NOTIFICATION_NOT_CAPABLE, SQL_CATALOG_NAME_SEPARATOR, SQL_CATALOG_TERM, SQL_CB_CLOSE,
    SQL_CURSOR_COMMIT_BEHAVIOR, SQL_CURSOR_ROLLBACK_BEHAVIOR, SQL_DATA_SOURCE_NAME,
    SQL_DATA_SOURCE_READ_ONLY, SQL_DBMS_NAME, SQL_DBMS_VER, SQL_DEFAULT_TXN_ISOLATION, SQL_DM_VER,
    SQL_DRIVER_NAME, SQL_DRIVER_ODBC_VER, SQL_DRIVER_VER, SQL_ERROR, SQL_EXPRESSIONS_IN_ORDERBY,
    SQL_FN_NONE_SUPPORTED, SQL_GD_ANY_COLUMN, SQL_GD_ANY_ORDER, SQL_GETDATA_EXTENSIONS,
    SQL_IDENTIFIER_QUOTE_CHAR, SQL_INVALID_HANDLE, SQL_KEYWORDS, SQL_MAX_COLUMN_NAME_LEN,
    SQL_MAX_DRIVER_CONNECTIONS, SQL_MAX_SCHEMA_NAME_LEN, SQL_MAX_STATEMENT_LEN,
    SQL_MAX_TABLE_NAME_LEN, SQL_MULTIPLE_ACTIVE_TXN, SQL_NEED_LONG_DATA_LEN, SQL_NUMERIC_FUNCTIONS,
    SQL_OAC_LEVEL2, SQL_ODBC_API_CONFORMANCE, SQL_ODBC_SQL_CONFORMANCE, SQL_ODBC_VER, SQL_OSC_CORE,
    SQL_PARAM_ARRAY_ROW_COUNTS, SQL_PARAM_ARRAY_SELECTS, SQL_PARC_NO_BATCH, SQL_PAS_BATCH,
    SQL_PROCEDURES, SQL_SC_SQL92_ENTRY, SQL_SCHEMA_TERM, SQL_SERVER_NAME, SQL_SPECIAL_CHARACTERS,
    SQL_SQL_CONFORMANCE, SQL_STRING_FUNCTIONS, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO,
    SQL_SYSTEM_FUNCTIONS, SQL_TC_ALL, SQL_TIMEDATE_FUNCTIONS, SQL_TXN_CAPABLE,
    SQL_TXN_ISOLATION_OPTION, SQL_TXN_ISOLATION_OPTION_SPT, SQL_TXN_READ_COMMITTED, SQL_USER_NAME,
    SqlHandle, SqlPointer, SqlReturn, SqlSmallInt, SqlUSmallInt, SqlWChar,
};
use crate::api::sqlstate::{ERR_INVALID_INFO_TYPE, WARN_STRING_TRUNCATION, post_diag};
use crate::api::util::{copy_with_nul, write_if_some};
use crate::error::free_errors;
use crate::handles::{DbcHandle, HandleType, handle_from_raw};

/// `sysname`, the type of every identifier column in the catalog views, which
/// bounds `SQL_MAX_COLUMN_NAME_LEN`, `SQL_MAX_SCHEMA_NAME_LEN`, and
/// `SQL_MAX_TABLE_NAME_LEN` alike.
const MAX_IDENTIFIER_LEN: u16 = 128;

const DEFAULT_PACKET_SIZE: u32 = 4096;
const MAX_SQL_BLOCKS: u32 = 128;

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

// TODO: This function implements only what is needed for
//       Windows ODBC Driver Manager to load the driver. Fix
//       hardcoded values and implement the rest of the info types.
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

    // Identity strings live behind the same borrow `write_wide_str` needs, so
    // take a copy before the call rather than restructuring the writer.
    let identity_value = match info_type {
        SQL_DATA_SOURCE_NAME => Some(state.identity.data_source_name.clone()),
        SQL_SERVER_NAME => Some(state.identity.server_name.clone()),
        SQL_USER_NAME => Some(state.identity.user_name.clone()),
        _ => None,
    };
    if let Some(value) = identity_value {
        return write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            &value,
        );
    }

    match info_type {
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
        SQL_MAX_STATEMENT_LEN => write_u32(
            info_value_ptr,
            max_statement_len(state.client.as_ref().map(|client| client.packet_size())),
            string_length_ptr,
        ),
        SQL_NUMERIC_FUNCTIONS
        | SQL_STRING_FUNCTIONS
        | SQL_SYSTEM_FUNCTIONS
        | SQL_TIMEDATE_FUNCTIONS => {
            write_u32(info_value_ptr, SQL_FN_NONE_SUPPORTED, string_length_ptr)
        }
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
        _ => {
            error!(info_type, "SQLGetInfoW: unsupported info type");
            post_diag(&mut state, ERR_INVALID_INFO_TYPE);
            SQL_ERROR
        }
    }
}

fn max_statement_len(packet_size: Option<u32>) -> u32 {
    MAX_SQL_BLOCKS * packet_size.unwrap_or(DEFAULT_PACKET_SIZE)
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
        (SQL_ACCESSIBLE_TABLES, "Y"),
        (SQL_ACCESSIBLE_PROCEDURES, "Y"),
        (SQL_EXPRESSIONS_IN_ORDERBY, "Y"),
        (SQL_DATA_SOURCE_READ_ONLY, "N"),
    ];

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
            (SQL_MAX_STATEMENT_LEN, 512 * 1024),
            (SQL_NUMERIC_FUNCTIONS, SQL_FN_NONE_SUPPORTED),
            (SQL_STRING_FUNCTIONS, SQL_FN_NONE_SUPPORTED),
            (SQL_SYSTEM_FUNCTIONS, SQL_FN_NONE_SUPPORTED),
            (SQL_TIMEDATE_FUNCTIONS, SQL_FN_NONE_SUPPORTED),
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
    fn max_statement_len_tracks_negotiated_packet_size() {
        assert_eq!(max_statement_len(None), 512 * 1024);
        assert_eq!(max_statement_len(Some(4096)), 512 * 1024);
        assert_eq!(max_statement_len(Some(8192)), 1024 * 1024);
        assert_eq!(max_statement_len(Some(32768)), 4 * 1024 * 1024);
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
