// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLGetInfoW.

use tracing::{debug, error};

use crate::api::odbc_types::{
    SQL_ACCESSIBLE_PROCEDURES, SQL_ACCESSIBLE_TABLES, SQL_ACTIVE_STATEMENTS,
    SQL_ASYNC_DBC_FUNCTIONS, SQL_ASYNC_DBC_NOT_CAPABLE, SQL_ASYNC_NOTIFICATION,
    SQL_ASYNC_NOTIFICATION_NOT_CAPABLE, SQL_CATALOG_NAME_SEPARATOR, SQL_CATALOG_TERM, SQL_CB_CLOSE,
    SQL_CURSOR_COMMIT_BEHAVIOR, SQL_CURSOR_ROLLBACK_BEHAVIOR, SQL_DATA_SOURCE_NAME,
    SQL_DATA_SOURCE_READ_ONLY, SQL_DATABASE_NAME, SQL_DBMS_NAME, SQL_DBMS_VER,
    SQL_DEFAULT_TXN_ISOLATION, SQL_DM_VER, SQL_DRIVER_NAME, SQL_DRIVER_ODBC_VER, SQL_DRIVER_VER,
    SQL_ERROR, SQL_EXPRESSIONS_IN_ORDERBY, SQL_FN_NUM_SPT, SQL_FN_STR_SPT, SQL_FN_SYS_SPT,
    SQL_FN_TD_SPT, SQL_GD_ANY_COLUMN, SQL_GD_ANY_ORDER, SQL_GETDATA_EXTENSIONS,
    SQL_IDENTIFIER_QUOTE_CHAR, SQL_INVALID_HANDLE, SQL_KEYWORDS, SQL_LIKE_ESCAPE_CLAUSE,
    SQL_MAX_CATALOG_NAME_LEN, SQL_MAX_COLUMN_NAME_LEN, SQL_MAX_DRIVER_CONNECTIONS,
    SQL_MAX_IDENTIFIER_LEN, SQL_MAX_SCHEMA_NAME_LEN, SQL_MAX_STATEMENT_LEN, SQL_MAX_TABLE_NAME_LEN,
    SQL_MULTIPLE_ACTIVE_TXN, SQL_NEED_LONG_DATA_LEN, SQL_NUMERIC_FUNCTIONS, SQL_OAC_LEVEL2,
    SQL_ODBC_API_CONFORMANCE, SQL_ODBC_SQL_CONFORMANCE, SQL_ODBC_VER, SQL_OSC_CORE,
    SQL_OUTER_JOINS, SQL_PROCEDURE_TERM, SQL_PROCEDURES, SQL_SC_SQL92_ENTRY, SQL_SCHEMA_TERM,
    SQL_SEARCH_PATTERN_ESCAPE, SQL_SERVER_NAME, SQL_SPECIAL_CHARACTERS, SQL_SQL_CONFORMANCE,
    SQL_STRING_FUNCTIONS, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO, SQL_SYSTEM_FUNCTIONS, SQL_TABLE_TERM,
    SQL_TC_ALL, SQL_TIMEDATE_FUNCTIONS, SQL_TXN_CAPABLE, SQL_TXN_ISOLATION_OPTION,
    SQL_TXN_ISOLATION_OPTION_SPT, SQL_TXN_READ_COMMITTED, SQL_USER_NAME, SqlHandle, SqlPointer,
    SqlReturn, SqlSmallInt, SqlUSmallInt, SqlWChar,
};
use crate::api::sqlstate::{ERR_INVALID_INFO_TYPE, WARN_STRING_TRUNCATION, post_diag};
use crate::api::util::{copy_with_nul, write_if_some};
use crate::error::free_errors;
use crate::handles::{DbcHandle, HandleType, handle_from_raw};

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
        SQL_MAX_DRIVER_CONNECTIONS => {
            // 0 means "no stated limit" per ODBC.
            write_u16(info_value_ptr, 0, string_length_ptr)
        }
        SQL_ACTIVE_STATEMENTS => write_u16(info_value_ptr, 0, string_length_ptr),
        SQL_DATA_SOURCE_NAME => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            "",
        ),
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
        SQL_DATABASE_NAME => {
            let database = state
                .client
                .as_ref()
                .map(|client| client.database().to_string())
                .or_else(|| state.current_catalog.clone())
                .unwrap_or_default();
            write_wide_str(
                &mut state,
                info_value_ptr,
                buffer_length,
                string_length_ptr,
                &database,
            )
        }
        SQL_SERVER_NAME => {
            let server_name = state.server_name.clone();
            write_wide_str(
                &mut state,
                info_value_ptr,
                buffer_length,
                string_length_ptr,
                &server_name,
            )
        }
        SQL_USER_NAME => {
            let user_name = state.user_name.clone();
            write_wide_str(
                &mut state,
                info_value_ptr,
                buffer_length,
                string_length_ptr,
                &user_name,
            )
        }
        SQL_IDENTIFIER_QUOTE_CHAR => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            "\"",
        ),
        SQL_SEARCH_PATTERN_ESCAPE => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            "\\",
        ),
        SQL_CATALOG_NAME_SEPARATOR => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            ".",
        ),
        SQL_CATALOG_TERM => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            "database",
        ),
        SQL_SCHEMA_TERM => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            "owner",
        ),
        SQL_TABLE_TERM => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            "table",
        ),
        SQL_PROCEDURE_TERM => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            "stored procedure",
        ),
        SQL_SPECIAL_CHARACTERS => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            "_#$",
        ),
        SQL_KEYWORDS => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            SQL_SERVER_KEYWORDS,
        ),
        SQL_ACCESSIBLE_TABLES
        | SQL_ACCESSIBLE_PROCEDURES
        | SQL_PROCEDURES
        | SQL_EXPRESSIONS_IN_ORDERBY
        | SQL_LIKE_ESCAPE_CLAUSE
        | SQL_OUTER_JOINS => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            "Y",
        ),
        SQL_DATA_SOURCE_READ_ONLY => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            "N",
        ),
        SQL_MAX_COLUMN_NAME_LEN
        | SQL_MAX_SCHEMA_NAME_LEN
        | SQL_MAX_CATALOG_NAME_LEN
        | SQL_MAX_TABLE_NAME_LEN
        | SQL_MAX_IDENTIFIER_LEN => write_u16(info_value_ptr, 128, string_length_ptr),
        SQL_MAX_STATEMENT_LEN => write_u32(
            info_value_ptr,
            state.packet_size.saturating_mul(128),
            string_length_ptr,
        ),
        SQL_NUMERIC_FUNCTIONS => write_u32(info_value_ptr, SQL_FN_NUM_SPT, string_length_ptr),
        SQL_STRING_FUNCTIONS => write_u32(info_value_ptr, SQL_FN_STR_SPT, string_length_ptr),
        SQL_SYSTEM_FUNCTIONS => write_u32(info_value_ptr, SQL_FN_SYS_SPT, string_length_ptr),
        SQL_TIMEDATE_FUNCTIONS => write_u32(info_value_ptr, SQL_FN_TD_SPT, string_length_ptr),
        SQL_SQL_CONFORMANCE => write_u32(info_value_ptr, SQL_SC_SQL92_ENTRY, string_length_ptr),
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
        _ => {
            error!(info_type, "SQLGetInfoW: unsupported info type");
            post_diag(&mut state, ERR_INVALID_INFO_TYPE);
            SQL_ERROR
        }
    }
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

const SQL_SERVER_KEYWORDS: &str = concat!(
    "BACKUP,BREAK,BROWSE,BULK,CHECKPOINT,CLUSTERED,COMMITTED,COMPUTE,CONFIRM,CONTROLROW,",
    "DATABASE,DBCC,DISK,DISTRIBUTED,DUMMY,ERRLVL,ERROREXIT,EXIT,FILE,FILLFACTOR,FLOPPY,",
    "HOLDLOCK,IDENTITY_INSERT,IDENTITYCOL,IF,KILL,LINENO,MERGE,MIRROREXIT,NONCLUSTERED,",
    "OFF,OFFSETS,ONCE,OVER,PERCENT,PERM,PERMANENT,PLAN,PRINT,PROC,PROCESSEXIT,RAISERROR,",
    "READ,READTEXT,RECONFIGURE,REPEATABLE,RESTORE,RETURN,ROWCOUNT,RULE,SAVE,SERIALIZABLE,",
    "SETUSER,SHUTDOWN,STATISTICS,TAPE,TEMP,TEXTSIZE,TOP,TRAN,TRIGGER,TRUNCATE,TSEQUEL,",
    "UNCOMMITTED,UPDATETEXT,USE,WAITFOR,WHILE,WRITETEXT",
);

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;
    use crate::api::odbc_types::{DEFAULT_PACKET_SIZE, SQL_NULL_HANDLE};
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

    fn get_wide(dbc: SqlHandle, info_type: SqlUSmallInt) -> (SqlReturn, String, SqlSmallInt) {
        let mut buf = [0u16; 1024];
        let mut len: SqlSmallInt = -1;
        let rc = unsafe {
            sql_get_info_w(
                dbc,
                info_type,
                buf.as_mut_ptr().cast(),
                (buf.len() * std::mem::size_of::<SqlWChar>()) as SqlSmallInt,
                &mut len,
            )
        };
        let units = usize::try_from(len.max(0)).unwrap_or(0) / std::mem::size_of::<SqlWChar>();
        (rc, String::from_utf16_lossy(&buf[..units]), len)
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
        ] {
            let (rc, val, len) = get_u32(h.dbc, info_type);
            assert_eq!(rc, SQL_SUCCESS, "info_type {info_type}");
            assert_eq!(val, expected, "info_type {info_type}");
            assert_eq!(len, 4, "info_type {info_type}");
        }
    }

    #[test]
    fn standard_string_info_types_report_sql_server_values() {
        let h = TestHandles::with_env_dbc();
        let dbc = unsafe { handle_from_raw::<DbcHandle>(h.dbc) };
        {
            let mut state = dbc.inner.lock().unwrap();
            state.server_name = "db.example.test,1433".to_string();
            state.user_name = "test-user".to_string();
        }

        for (info_type, expected) in [
            (SQL_DATA_SOURCE_NAME, ""),
            (SQL_SERVER_NAME, "db.example.test,1433"),
            (SQL_USER_NAME, "test-user"),
            (SQL_SEARCH_PATTERN_ESCAPE, "\\"),
            (SQL_CATALOG_NAME_SEPARATOR, "."),
            (SQL_CATALOG_TERM, "database"),
            (SQL_SCHEMA_TERM, "owner"),
            (SQL_TABLE_TERM, "table"),
            (SQL_PROCEDURE_TERM, "stored procedure"),
            (SQL_SPECIAL_CHARACTERS, "_#$"),
            (SQL_ACCESSIBLE_TABLES, "Y"),
            (SQL_ACCESSIBLE_PROCEDURES, "Y"),
            (SQL_PROCEDURES, "Y"),
            (SQL_EXPRESSIONS_IN_ORDERBY, "Y"),
            (SQL_LIKE_ESCAPE_CLAUSE, "Y"),
            (SQL_OUTER_JOINS, "Y"),
            (SQL_DATA_SOURCE_READ_ONLY, "N"),
        ] {
            let (rc, value, len) = get_wide(h.dbc, info_type);
            assert_eq!(rc, SQL_SUCCESS, "info_type {info_type}");
            assert_eq!(value, expected, "info_type {info_type}");
            assert_eq!(
                usize::try_from(len).unwrap(),
                expected.encode_utf16().count() * std::mem::size_of::<SqlWChar>(),
                "info_type {info_type}"
            );
        }
        let (rc, keywords, _) = get_wide(h.dbc, SQL_KEYWORDS);
        assert_eq!(rc, SQL_SUCCESS);
        assert!(keywords.contains("BACKUP"));
        assert!(keywords.contains("WRITETEXT"));
    }

    #[test]
    fn standard_numeric_info_types_report_sql_server_values() {
        let h = TestHandles::with_env_dbc();
        for info_type in [
            SQL_MAX_COLUMN_NAME_LEN,
            SQL_MAX_SCHEMA_NAME_LEN,
            SQL_MAX_CATALOG_NAME_LEN,
            SQL_MAX_TABLE_NAME_LEN,
            SQL_MAX_IDENTIFIER_LEN,
        ] {
            let (rc, value, len) = get_u16(h.dbc, info_type);
            assert_eq!((rc, value, len), (SQL_SUCCESS, 128, 2));
        }
        for (info_type, expected) in [
            (SQL_MAX_STATEMENT_LEN, 128 * DEFAULT_PACKET_SIZE),
            (SQL_NUMERIC_FUNCTIONS, SQL_FN_NUM_SPT),
            (SQL_STRING_FUNCTIONS, SQL_FN_STR_SPT),
            (SQL_SYSTEM_FUNCTIONS, SQL_FN_SYS_SPT),
            (SQL_TIMEDATE_FUNCTIONS, SQL_FN_TD_SPT),
            (SQL_SQL_CONFORMANCE, SQL_SC_SQL92_ENTRY),
        ] {
            let (rc, value, len) = get_u32(h.dbc, info_type);
            assert_eq!((rc, value, len), (SQL_SUCCESS, expected, 4));
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
}
