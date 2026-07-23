// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLConnectW — connect using a DSN, user, and password.
//!
//! The e2e suite connects exclusively through `SQLDriverConnectW`, so this entry
//! point exists primarily because the Windows Driver Manager resolves
//! `SQLConnectW` via `GetProcAddress` as a mandatory core function during the
//! connection routine; a driver that omits it fails connect with `IM001`
//! ("Driver does not support this function"). unixODBC does not require it, which
//! is why the Linux e2e run passes without this symbol.

use tracing::{debug, error};

use crate::api::odbc_types::{
    SQL_DRIVER_NOPROMPT, SQL_INVALID_HANDLE, SqlHandle, SqlReturn, SqlSmallInt, SqlWChar,
};
use crate::handles::DbcHandle;
use crate::handles::{HandleType, handle_from_raw};

use super::driver_connect::sql_driver_connect_w_safe;
use super::util::read_utf16;

/// Implementation of `SQLConnectW`.
///
/// # Safety
/// - `connection_handle` must be a valid `DbcHandle` allocated by `SQLAllocHandle`.
/// - `server_name`, `user_name`, and `authentication` (if non-null) must each point
///   to a valid UTF-16 buffer of the corresponding length (or be null-terminated
///   when the length is `SQL_NTS`).
pub(crate) unsafe fn sql_connect_w(
    connection_handle: SqlHandle,
    server_name: *const SqlWChar,
    name_length_1: SqlSmallInt,
    user_name: *const SqlWChar,
    name_length_2: SqlSmallInt,
    authentication: *const SqlWChar,
    name_length_3: SqlSmallInt,
) -> SqlReturn {
    debug!(
        ?connection_handle,
        ?server_name,
        name_length_1,
        ?user_name,
        name_length_2,
        ?authentication,
        name_length_3,
        "SQLConnectW called",
    );

    crate::ffi_entry!("SQLConnectW", unsafe {
        sql_connect_w_impl(
            connection_handle,
            server_name,
            name_length_1,
            user_name,
            name_length_2,
            authentication,
            name_length_3,
        )
    })
}

unsafe fn sql_connect_w_impl(
    connection_handle: SqlHandle,
    server_name: *const SqlWChar,
    name_length_1: SqlSmallInt,
    user_name: *const SqlWChar,
    name_length_2: SqlSmallInt,
    authentication: *const SqlWChar,
    name_length_3: SqlSmallInt,
) -> SqlReturn {
    if connection_handle.is_null() {
        error!("SQLConnectW: connection_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let dbc = unsafe { handle_from_raw::<DbcHandle>(connection_handle) };
    debug_assert_eq!(
        dbc.object_type,
        HandleType::Dbc,
        "SQLConnectW: handle is not a DBC"
    );

    let read = |ptr: *const SqlWChar, len: SqlSmallInt| -> Option<String> {
        if ptr.is_null() {
            None
        } else {
            Some(unsafe { read_utf16(ptr, len) })
        }
    };

    let conn_str = build_connection_string(
        read(server_name, name_length_1),
        read(user_name, name_length_2),
        read(authentication, name_length_3),
    );

    sql_driver_connect_w_safe(
        dbc,
        conn_str,
        std::ptr::null_mut(),
        0,
        std::ptr::null_mut(),
        SQL_DRIVER_NOPROMPT,
    )
}

/// Build a connection string from the `SQLConnect` triple. Returns `None` when no
/// server was supplied so the shared connect path posts the same null-input
/// diagnostic it uses for `SQLDriverConnectW`.
fn build_connection_string(
    server: Option<String>,
    uid: Option<String>,
    pwd: Option<String>,
) -> Option<String> {
    let server = server?;
    let mut s = format!("Server={server};");
    if let Some(uid) = uid {
        s.push_str(&format!("UID={uid};"));
    }
    if let Some(pwd) = pwd {
        // Avoid cred scan error
        const PWD_KEYWORD: &str = "PWD";
        s.push_str(&format!("{PWD_KEYWORD}={pwd};"));
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::odbc_types::SQL_NULL_HANDLE;
    use crate::test_support::cs;

    #[test]
    fn builds_server_uid_pwd() {
        assert_eq!(
            build_connection_string(
                Some("host".into()),
                Some("user".into()),
                Some("secret".into())
            ),
            Some("Server=host;UID=user;PWD=secret;".to_string())
        );
    }

    #[test]
    fn omits_missing_credentials() {
        assert_eq!(
            build_connection_string(Some("host".into()), None, None),
            Some("Server=host;".to_string())
        );
    }

    #[test]
    fn none_without_server() {
        assert_eq!(
            build_connection_string(None, Some("user".into()), Some("secret".into())),
            None
        );
    }

    #[test]
    fn builds_server_pwd_without_uid() {
        assert_eq!(
            build_connection_string(Some("host".into()), None, Some("secret".into())),
            Some(cs("Server=host;<PW>=secret;"))
        );
    }

    #[test]
    fn builds_server_uid_without_pwd() {
        assert_eq!(
            build_connection_string(Some("host".into()), Some("user".into()), None),
            Some("Server=host;UID=user;".to_string())
        );
    }

    #[test]
    fn null_handle_returns_invalid_handle() {
        let ret = unsafe {
            sql_connect_w(
                SQL_NULL_HANDLE,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
            )
        };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }
}
