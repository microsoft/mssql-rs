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
use crate::api::sqlstate::{ERR_STRING_RIGHT_TRUNCATION, post_diag};
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
        SQL_DBMS_VER => write_wide_str(
            &mut state,
            info_value_ptr,
            buffer_length,
            string_length_ptr,
            "16.00.0000",
        ),
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
        _ => {
            // Keep unsupported info-types explicit; callers can probe support
            // by handling SQL_ERROR.
            error!(info_type, "SQLGetInfoW: unsupported info type");
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
    }
}
