// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLSetEnvAttr.
//!
//! Mirrors msodbcsql's `SQLSetEnvAttr`, replacing its `dwOptionsE[]` table
//! with typed fields on `EnvState`. The DM owns HY010 enforcement.

use std::panic;

use tracing::{debug, error, trace};

use crate::api::odbc_types::{
    SQL_ATTR_ODBC_VERSION, SQL_ERROR, SQL_INVALID_HANDLE, SQL_SUCCESS, SqlHandle, SqlInteger,
    SqlPointer, SqlReturn,
};
use crate::handles::{EnvHandle, HandleType, OdbcVersion, handle_from_raw};

/// Sets an attribute on an environment handle.
///
/// # Safety
/// - `environment_handle` must be a valid `EnvHandle` from `SQLAllocHandle`.
/// - `value_ptr` is an ODBC tagged integer for integer attributes.
pub(crate) unsafe fn sql_set_env_attr(
    environment_handle: SqlHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
    _string_length: SqlInteger,
) -> SqlReturn {
    debug!(
        ?environment_handle,
        attribute,
        ?value_ptr,
        "SQLSetEnvAttr called"
    );

    let result = panic::catch_unwind(|| {
        if environment_handle.is_null() {
            error!("SQLSetEnvAttr: environment_handle is null");
            return SQL_INVALID_HANDLE;
        }

        let env = unsafe { handle_from_raw::<EnvHandle>(environment_handle) };
        debug_assert_eq!(
            env.header.object_type,
            HandleType::Env,
            "SQLSetEnvAttr: input_handle is not an ENV handle"
        );

        let Ok(mut state) = env.inner.lock() else {
            error!("SQLSetEnvAttr: env mutex poisoned");
            return SQL_ERROR;
        };

        // TODO: equivalent of msodbcsql `FreeErrors(lpEnv)` — clear prior
        // diagnostic records on the handle. Deferred until diag infra lands.

        // ODBC tagged-pointer: integer values arrive as `(SQLPOINTER)(uintptr_t)value`.
        let value = value_ptr as usize as u32;

        match attribute {
            SQL_ATTR_ODBC_VERSION => match OdbcVersion::try_from(value) {
                Ok(v) => {
                    state.odbc_version = v;
                    SQL_SUCCESS
                }
                Err(()) => {
                    error!(value, "SQLSetEnvAttr: invalid ODBC_VERSION value");
                    // TODO: SQLSTATE HY024 (invalid attribute value) when diag infra lands.
                    SQL_ERROR
                }
            },
            _ => {
                error!(attribute, "SQLSetEnvAttr: unknown attribute");
                // TODO: SQLSTATE HY092 (invalid attribute identifier).
                SQL_ERROR
            }
        }
    });

    let ret = result.unwrap_or_else(|_| {
        error!("SQLSetEnvAttr: panic caught at FFI boundary");
        SQL_ERROR
    });

    trace!(?ret, "SQLSetEnvAttr returning");
    ret
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;
    use crate::api::alloc_handle::sql_alloc_handle;
    use crate::api::free_handle::sql_free_handle;
    use crate::api::odbc_types::{
        SQL_HANDLE_ENV, SQL_NULL_HANDLE, SQL_OV_ODBC2, SQL_OV_ODBC3, SQL_OV_ODBC3_80,
    };

    fn alloc_env() -> SqlHandle {
        let mut h: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_ENV, SQL_NULL_HANDLE, &mut h) };
        assert_eq!(ret, SQL_SUCCESS);
        h
    }

    fn free_env(h: SqlHandle) {
        unsafe { sql_free_handle(SQL_HANDLE_ENV, h) };
    }

    fn set_attr(env: SqlHandle, attr: SqlInteger, value: u32) -> SqlReturn {
        unsafe { sql_set_env_attr(env, attr, value as usize as SqlPointer, 0) }
    }

    #[test]
    fn set_odbc_version_3_80_success() {
        let env = alloc_env();
        let ret = set_attr(env, SQL_ATTR_ODBC_VERSION, SQL_OV_ODBC3_80);
        assert_eq!(ret, SQL_SUCCESS);
        let env_ref = unsafe { &*(env as *const EnvHandle) };
        assert_eq!(
            env_ref.inner.lock().unwrap().odbc_version,
            OdbcVersion::Odbc3_80
        );
        free_env(env);
    }

    #[test]
    fn set_odbc_version_3_success() {
        let env = alloc_env();
        let ret = set_attr(env, SQL_ATTR_ODBC_VERSION, SQL_OV_ODBC3);
        assert_eq!(ret, SQL_SUCCESS);
        let env_ref = unsafe { &*(env as *const EnvHandle) };
        assert_eq!(
            env_ref.inner.lock().unwrap().odbc_version,
            OdbcVersion::Odbc3
        );
        free_env(env);
    }

    #[test]
    fn set_odbc_version_2_success() {
        let env = alloc_env();
        let ret = set_attr(env, SQL_ATTR_ODBC_VERSION, SQL_OV_ODBC2);
        assert_eq!(ret, SQL_SUCCESS);
        let env_ref = unsafe { &*(env as *const EnvHandle) };
        assert_eq!(
            env_ref.inner.lock().unwrap().odbc_version,
            OdbcVersion::Odbc2
        );
        free_env(env);
    }

    #[test]
    fn set_odbc_version_invalid_value() {
        let env = alloc_env();
        let ret = set_attr(env, SQL_ATTR_ODBC_VERSION, 9999);
        assert_eq!(ret, SQL_ERROR);
        let env_ref = unsafe { &*(env as *const EnvHandle) };
        assert_eq!(
            env_ref.inner.lock().unwrap().odbc_version,
            OdbcVersion::Unset
        );
        free_env(env);
    }

    #[test]
    fn set_env_attr_null_handle_invalid() {
        let ret = unsafe {
            sql_set_env_attr(
                ptr::null_mut(),
                SQL_ATTR_ODBC_VERSION,
                SQL_OV_ODBC3_80 as usize as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }

    #[test]
    fn set_env_attr_unknown_attribute_error() {
        let env = alloc_env();
        let ret = set_attr(env, 12345, 0);
        assert_eq!(ret, SQL_ERROR);
        free_env(env);
    }

    #[test]
    fn set_odbc_version_overwrites_previous() {
        // ODBC apps may call SQLSetEnvAttr multiple times before allocating a
        // DBC; the last write wins.
        let env = alloc_env();
        assert_eq!(
            set_attr(env, SQL_ATTR_ODBC_VERSION, SQL_OV_ODBC2),
            SQL_SUCCESS
        );
        assert_eq!(
            set_attr(env, SQL_ATTR_ODBC_VERSION, SQL_OV_ODBC3_80),
            SQL_SUCCESS
        );
        let env_ref = unsafe { &*(env as *const EnvHandle) };
        assert_eq!(
            env_ref.inner.lock().unwrap().odbc_version,
            OdbcVersion::Odbc3_80
        );
        free_env(env);
    }

    #[test]
    fn set_invalid_version_preserves_previous_value() {
        // A rejected SQLSetEnvAttr must not corrupt previously-stored state.
        let env = alloc_env();
        assert_eq!(
            set_attr(env, SQL_ATTR_ODBC_VERSION, SQL_OV_ODBC3_80),
            SQL_SUCCESS
        );
        assert_eq!(set_attr(env, SQL_ATTR_ODBC_VERSION, 9999), SQL_ERROR);
        let env_ref = unsafe { &*(env as *const EnvHandle) };
        assert_eq!(
            env_ref.inner.lock().unwrap().odbc_version,
            OdbcVersion::Odbc3_80
        );
        free_env(env);
    }

    #[test]
    fn string_length_is_ignored_for_integer_attributes() {
        // ODBC spec: StringLength is ignored for fixed-length / integer
        // attributes. Verify a nonsense length still yields SQL_SUCCESS.
        let env = alloc_env();
        let ret = unsafe {
            sql_set_env_attr(
                env,
                SQL_ATTR_ODBC_VERSION,
                SQL_OV_ODBC3_80 as usize as SqlPointer,
                123456,
            )
        };
        assert_eq!(ret, SQL_SUCCESS);
        let env_ref = unsafe { &*(env as *const EnvHandle) };
        assert_eq!(
            env_ref.inner.lock().unwrap().odbc_version,
            OdbcVersion::Odbc3_80
        );
        free_env(env);
    }
}
