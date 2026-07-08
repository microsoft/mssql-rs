// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLSetEnvAttr.
//!
//! Mirrors msodbcsql's `SQLSetEnvAttr`, replacing its internal options table
//! with typed fields on `EnvState`. The DM owns HY010 enforcement.

use tracing::{debug, error};

use super::sqlstate::*;
use crate::api::odbc_types::{
    SQL_ATTR_ODBC_VERSION, SQL_ERROR, SQL_INVALID_HANDLE, SQL_SUCCESS, SqlHandle, SqlInteger,
    SqlPointer, SqlReturn,
};
use crate::error::free_errors;
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
        "SQLSetEnvAttr called",
    );

    crate::ffi_entry!("SQLSetEnvAttr", unsafe {
        sql_set_env_attr_impl(environment_handle, attribute, value_ptr)
    })
}

unsafe fn sql_set_env_attr_impl(
    environment_handle: SqlHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
) -> SqlReturn {
    if environment_handle.is_null() {
        error!("SQLSetEnvAttr: environment_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let env = unsafe { handle_from_raw::<EnvHandle>(environment_handle) };
    debug_assert_eq!(
        env.object_type,
        HandleType::Env,
        "SQLSetEnvAttr: input_handle is not an ENV handle"
    );

    sql_set_env_attr_safe(env, attribute, value_ptr)
}

fn sql_set_env_attr_safe(
    env: &EnvHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
) -> SqlReturn {
    let Ok(mut state) = env.inner.lock() else {
        error!("SQLSetEnvAttr: env mutex poisoned");
        return SQL_ERROR;
    };

    free_errors(&mut state);

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
                post_diag(&mut state, ERR_INVALID_ATTRIBUTE_VALUE);
                SQL_ERROR
            }
        },
        _ => {
            error!(attribute, "SQLSetEnvAttr: unknown attribute");
            post_diag(&mut state, ERR_INVALID_ATTRIBUTE_IDENTIFIER);
            SQL_ERROR
        }
    }
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

    #[test]
    fn invalid_version_posts_hy024_diag() {
        let env = alloc_env();
        assert_eq!(set_attr(env, SQL_ATTR_ODBC_VERSION, 9999), SQL_ERROR);
        let env_ref = unsafe { &*(env as *const EnvHandle) };
        let state = env_ref.inner.lock().unwrap();
        assert_eq!(state.diag_records.len(), 1);
        assert_eq!(&state.diag_records[0].sql_state, b"HY024");
        drop(state);
        free_env(env);
    }

    #[test]
    fn unknown_attribute_posts_hy092_diag() {
        let env = alloc_env();
        assert_eq!(set_attr(env, 12345, 0), SQL_ERROR);
        let env_ref = unsafe { &*(env as *const EnvHandle) };
        let state = env_ref.inner.lock().unwrap();
        assert_eq!(state.diag_records.len(), 1);
        assert_eq!(&state.diag_records[0].sql_state, b"HY092");
        drop(state);
        free_env(env);
    }

    #[test]
    fn successful_call_clears_prior_diag_records() {
        let env = alloc_env();
        assert_eq!(set_attr(env, SQL_ATTR_ODBC_VERSION, 9999), SQL_ERROR);
        let env_ref = unsafe { &*(env as *const EnvHandle) };
        assert_eq!(env_ref.inner.lock().unwrap().diag_records.len(), 1);
        assert_eq!(
            set_attr(env, SQL_ATTR_ODBC_VERSION, SQL_OV_ODBC3_80),
            SQL_SUCCESS
        );
        assert!(env_ref.inner.lock().unwrap().diag_records.is_empty());
        free_env(env);
    }
}
