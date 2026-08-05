// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLGetInfoW.

use tracing::{debug, error};

use crate::api::odbc_types::{
    SQL_ACTIVE_STATEMENTS, SQL_ASYNC_DBC_FUNCTIONS, SQL_ASYNC_DBC_NOT_CAPABLE,
    SQL_ASYNC_NOTIFICATION, SQL_ASYNC_NOTIFICATION_NOT_CAPABLE, SQL_CB_CLOSE,
    SQL_CURSOR_COMMIT_BEHAVIOR, SQL_CURSOR_ROLLBACK_BEHAVIOR, SQL_DBMS_NAME, SQL_DBMS_VER,
    SQL_DM_VER, SQL_DRIVER_NAME, SQL_DRIVER_ODBC_VER, SQL_DRIVER_VER, SQL_ERROR, SQL_GD_ANY_COLUMN,
    SQL_GD_ANY_ORDER, SQL_GETDATA_EXTENSIONS, SQL_IDENTIFIER_QUOTE_CHAR, SQL_INVALID_HANDLE,
    SQL_MAX_DRIVER_CONNECTIONS, SQL_NEED_LONG_DATA_LEN, SQL_OAC_LEVEL2, SQL_ODBC_API_CONFORMANCE,
    SQL_ODBC_SQL_CONFORMANCE, SQL_ODBC_VER, SQL_OSC_CORE, SQL_SUCCESS, SQL_SUCCESS_WITH_INFO,
    SqlHandle, SqlPointer, SqlReturn, SqlSmallInt, SqlUSmallInt, SqlWChar,
};
use crate::api::sqlstate::{ERR_INVALID_INFO_TYPE, ERR_STRING_RIGHT_TRUNCATION, post_diag};
use crate::api::util::{copy_with_nul, write_if_some};
use crate::error::free_errors;
use crate::handles::{DbcHandle, HandleType, handle_from_raw};

const SQL_DATA_SOURCE_NAME: SqlUSmallInt = 2;
const SQL_SERVER_NAME: SqlUSmallInt = 13;
const SQL_SEARCH_PATTERN_ESCAPE: SqlUSmallInt = 14;
const SQL_ACCESSIBLE_TABLES: SqlUSmallInt = 19;
const SQL_ACCESSIBLE_PROCEDURES: SqlUSmallInt = 20;
const SQL_PROCEDURES: SqlUSmallInt = 21;
const SQL_DATA_SOURCE_READ_ONLY: SqlUSmallInt = 25;
const SQL_DEFAULT_TXN_ISOLATION: SqlUSmallInt = 26;
const SQL_EXPRESSIONS_IN_ORDERBY: SqlUSmallInt = 27;
const SQL_MAX_COLUMN_NAME_LEN: SqlUSmallInt = 30;
const SQL_MAX_SCHEMA_NAME_LEN: SqlUSmallInt = 32;
const SQL_MAX_CATALOG_NAME_LEN: SqlUSmallInt = 34;
const SQL_MAX_TABLE_NAME_LEN: SqlUSmallInt = 35;
const SQL_MULTIPLE_ACTIVE_TXN: SqlUSmallInt = 37;
const SQL_OUTER_JOINS: SqlUSmallInt = 38;
const SQL_SCHEMA_TERM: SqlUSmallInt = 39;
const SQL_PROCEDURE_TERM: SqlUSmallInt = 40;
const SQL_CATALOG_NAME_SEPARATOR: SqlUSmallInt = 41;
const SQL_CATALOG_TERM: SqlUSmallInt = 42;
const SQL_TABLE_TERM: SqlUSmallInt = 45;
const SQL_TXN_CAPABLE: SqlUSmallInt = 46;
const SQL_USER_NAME: SqlUSmallInt = 47;
const SQL_NUMERIC_FUNCTIONS: SqlUSmallInt = 49;
const SQL_STRING_FUNCTIONS: SqlUSmallInt = 50;
const SQL_DATETIME_FUNCTIONS: SqlUSmallInt = 51;
const SQL_KEYWORDS: SqlUSmallInt = 89;
const SQL_SPECIAL_CHARACTERS: SqlUSmallInt = 94;
const SQL_MAX_STATEMENT_LEN: SqlUSmallInt = 105;
const SQL_LIKE_ESCAPE_CLAUSE: SqlUSmallInt = 113;
const SQL_SQL_CONFORMANCE: SqlUSmallInt = 118;
const SQL_MAX_IDENTIFIER_LEN: SqlUSmallInt = 10005;

const SQL_TC_ALL: u16 = 2;
const SQL_SC_SQL92_ENTRY: u32 = 0x0000_0001;
const SQL_FN_NUM_SPT: u32 = 0x00FF_FFFF;
const SQL_FN_STR_SPT: u32 = 0x004F_FFFF;
const SQL_FN_TD_SPT: u32 = 0x001F_FFFF;

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
        SQL_SERVER_NAME => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            "localhost",
        ),
        SQL_SEARCH_PATTERN_ESCAPE => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            "\\",
        ),
        SQL_ACCESSIBLE_TABLES
        | SQL_ACCESSIBLE_PROCEDURES
        | SQL_EXPRESSIONS_IN_ORDERBY
        | SQL_MULTIPLE_ACTIVE_TXN
        | SQL_OUTER_JOINS
        | SQL_PROCEDURES
        | SQL_LIKE_ESCAPE_CLAUSE
        | SQL_NEED_LONG_DATA_LEN => write_wide_str(
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
        SQL_DEFAULT_TXN_ISOLATION => write_u32(
            info_value_ptr,
            crate::api::odbc_types::SQL_TXN_READ_COMMITTED,
            string_length_ptr,
        ),
        SQL_MAX_COLUMN_NAME_LEN
        | SQL_MAX_SCHEMA_NAME_LEN
        | SQL_MAX_CATALOG_NAME_LEN
        | SQL_MAX_TABLE_NAME_LEN
        | SQL_MAX_IDENTIFIER_LEN => write_u16(info_value_ptr, 128, string_length_ptr),
        SQL_MAX_STATEMENT_LEN => write_u32(info_value_ptr, 0, string_length_ptr),
        SQL_SCHEMA_TERM => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            "owner",
        ),
        SQL_PROCEDURE_TERM => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            "stored procedure",
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
        SQL_TABLE_TERM => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            "table",
        ),
        SQL_TXN_CAPABLE => write_u16(info_value_ptr, SQL_TC_ALL, string_length_ptr),
        SQL_USER_NAME => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            "",
        ),
        SQL_NUMERIC_FUNCTIONS => write_u32(info_value_ptr, SQL_FN_NUM_SPT, string_length_ptr),
        SQL_STRING_FUNCTIONS => write_u32(info_value_ptr, SQL_FN_STR_SPT, string_length_ptr),
        SQL_DATETIME_FUNCTIONS => write_u32(info_value_ptr, SQL_FN_TD_SPT, string_length_ptr),
        SQL_KEYWORDS | SQL_SPECIAL_CHARACTERS => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            "",
        ),
        SQL_SQL_CONFORMANCE => write_u32(info_value_ptr, SQL_SC_SQL92_ENTRY, string_length_ptr),
        SQL_ODBC_API_CONFORMANCE => write_u16(info_value_ptr, SQL_OAC_LEVEL2, string_length_ptr),
        SQL_ODBC_SQL_CONFORMANCE => write_u16(info_value_ptr, SQL_OSC_CORE, string_length_ptr),
        SQL_CURSOR_COMMIT_BEHAVIOR => write_u16(info_value_ptr, SQL_CB_CLOSE, string_length_ptr),
        SQL_CURSOR_ROLLBACK_BEHAVIOR => write_u16(info_value_ptr, SQL_CB_CLOSE, string_length_ptr),
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
        post_diag(state, ERR_STRING_RIGHT_TRUNCATION);
        SQL_SUCCESS_WITH_INFO
    } else {
        SQL_SUCCESS
    }
}

#[cfg(target_os = "windows")]
fn driver_name() -> &'static str {
    "msodbcsql18.dll"
}

#[cfg(target_os = "linux")]
fn driver_name() -> &'static str {
    "libmsodbcsql18.so"
}

#[cfg(target_os = "macos")]
fn driver_name() -> &'static str {
    "libmsodbcsql18.dylib"
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
            (SQL_MAX_COLUMN_NAME_LEN, 128),
            (SQL_MAX_SCHEMA_NAME_LEN, 128),
            (SQL_MAX_CATALOG_NAME_LEN, 128),
            (SQL_MAX_TABLE_NAME_LEN, 128),
            (SQL_MAX_IDENTIFIER_LEN, 128),
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
            (
                SQL_DEFAULT_TXN_ISOLATION,
                crate::api::odbc_types::SQL_TXN_READ_COMMITTED,
            ),
            (SQL_MAX_STATEMENT_LEN, 0),
            (SQL_NUMERIC_FUNCTIONS, SQL_FN_NUM_SPT),
            (SQL_STRING_FUNCTIONS, SQL_FN_STR_SPT),
            (SQL_DATETIME_FUNCTIONS, SQL_FN_TD_SPT),
            (SQL_SQL_CONFORMANCE, SQL_SC_SQL92_ENTRY),
        ] {
            let (rc, val, len) = get_u32(h.dbc, info_type);
            assert_eq!(rc, SQL_SUCCESS, "info_type {info_type}");
            assert_eq!(val, expected, "info_type {info_type}");
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
        let expected = driver_name();
        assert_eq!(len, (expected.encode_utf16().count() * 2) as SqlSmallInt);
        let n = (len as usize) / 2;
        assert_eq!(String::from_utf16_lossy(&buf[..n]), expected);
        // Null-terminated just past the copied text.
        assert_eq!(buf[n], 0);
    }

    #[test]
    fn getinfo_string_table_reports_expected_values() {
        let h = TestHandles::with_env_dbc();
        for (info_type, expected) in [
            (SQL_SERVER_NAME, "localhost"),
            (SQL_SEARCH_PATTERN_ESCAPE, "\\"),
            (SQL_ACCESSIBLE_TABLES, "Y"),
            (SQL_ACCESSIBLE_PROCEDURES, "Y"),
            (SQL_PROCEDURES, "Y"),
            (SQL_DATA_SOURCE_READ_ONLY, "N"),
            (SQL_EXPRESSIONS_IN_ORDERBY, "Y"),
            (SQL_MULTIPLE_ACTIVE_TXN, "Y"),
            (SQL_OUTER_JOINS, "Y"),
            (SQL_SCHEMA_TERM, "owner"),
            (SQL_PROCEDURE_TERM, "stored procedure"),
            (SQL_CATALOG_NAME_SEPARATOR, "."),
            (SQL_CATALOG_TERM, "database"),
            (SQL_TABLE_TERM, "table"),
            (SQL_LIKE_ESCAPE_CLAUSE, "Y"),
            (SQL_NEED_LONG_DATA_LEN, "Y"),
        ] {
            let mut buf = [0u16; 32];
            let mut len: SqlSmallInt = -1;
            let rc = unsafe {
                sql_get_info_w(
                    h.dbc,
                    info_type,
                    buf.as_mut_ptr() as SqlPointer,
                    (buf.len() * std::mem::size_of::<SqlWChar>()) as SqlSmallInt,
                    &mut len,
                )
            };
            assert_eq!(rc, SQL_SUCCESS, "info_type {info_type}");
            let n = (len as usize) / 2;
            assert_eq!(String::from_utf16_lossy(&buf[..n]), expected);
        }
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
            ERR_STRING_RIGHT_TRUNCATION.state
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
