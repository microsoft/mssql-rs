// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLSetEnvAttr — sets an environment attribute.
//!
//! Equivalent to msodbcsql's `SQLSetEnvAttr`. msodbcsql stores attributes as
//! `UINT_PTR` in a flat `ENV::dwOptionsE[NUM_TOTAL_ENV_OPTS]` array indexed
//! by `fAttribute - SQL_ENV_OPT_MIN`; we replace that array with typed fields
//! on `EnvState` and dispatch via an explicit match. Result is the same with
//! stronger compile-time guarantees and eager value validation.
//!
//! The Driver Manager owns HY010 (function sequence) enforcement, so we do
//! not re-check it here.

use std::panic;

use tracing::{debug, error, trace};

use crate::api::odbc_types::{
    SQL_ATTR_ODBC_VERSION, SQL_ERROR, SQL_INVALID_HANDLE, SQL_OV_ODBC2, SQL_OV_ODBC3,
    SQL_OV_ODBC3_80, SQL_SUCCESS, SqlHandle, SqlInteger, SqlPointer, SqlReturn,
};
use crate::handles::{EnvHandle, HandleType, OdbcVersion, handle_from_raw};

/// Sets an attribute on an environment handle.
///
/// # Safety
/// Called from C via the ODBC Driver Manager.
/// - `environment_handle` must be a valid `EnvHandle` previously returned by `SQLAllocHandle`.
/// - `value_ptr` is interpreted as a tagged integer (ODBC convention for integer attributes).
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

        // msodbcsql: `LPENV lpEnv = static_cast<LPENV>(hEnv);` (no null/type
        // check — DM is trusted). We additionally validate the runtime type
        // tag set up by SQLAllocHandle, which catches type-confused handles.
        let env = unsafe { handle_from_raw::<EnvHandle>(environment_handle) };
        if env.header.object_type != HandleType::Env {
            error!(
                ?environment_handle,
                "SQLSetEnvAttr: handle is not an ENV handle"
            );
            return SQL_INVALID_HANDLE;
        }

        // msodbcsql: `CMPCSAutoBlock csEnv(&lpEnv->csEnv);` — RAII lock on the
        // env's critical section, serializing concurrent SQLSetEnvAttr /
        // SQLAllocHandle(DBC) calls on the same HENV. We hold a `MutexGuard`
        // instead; it is released at scope exit.
        let Ok(mut state) = env.inner.lock() else {
            error!("SQLSetEnvAttr: env mutex poisoned");
            return SQL_ERROR;
        };

        // TODO: equivalent of msodbcsql `FreeErrors(lpEnv)` — clear prior
        // diagnostic records on the handle. Deferred until diag infra lands.

        // ODBC tagged-pointer convention: integer attribute values are passed
        // as `(SQLPOINTER)(uintptr_t)value`. msodbcsql stores the pointer
        // verbatim as `UINT_PTR` in `dwOptionsE[index]`; we recover the low
        // 32 bits and dispatch on the attribute.
        let value = value_ptr as usize as u32;

        // msodbcsql normalizes `fAttribute` into a `dwOptionsE[]` index and
        // bounds-checks against `NUMELEM(dwOptionsE)`, on failure invoking
        // `SETRC_SERR_GOTO` (set `SQL_ERROR`, goto RetExit). The equivalent
        // here is the `_ => SQL_ERROR` arm below; typed fields replace the
        // raw integer table.
        match attribute {
            SQL_ATTR_ODBC_VERSION => match value {
                SQL_OV_ODBC2 => {
                    state.odbc_version = OdbcVersion::Odbc2;
                    SQL_SUCCESS
                }
                SQL_OV_ODBC3 => {
                    state.odbc_version = OdbcVersion::Odbc3;
                    SQL_SUCCESS
                }
                SQL_OV_ODBC3_80 => {
                    state.odbc_version = OdbcVersion::Odbc3_80;
                    SQL_SUCCESS
                }
                _ => {
                    error!(value, "SQLSetEnvAttr: invalid ODBC_VERSION value");
                    // TODO: SQLSTATE HY024 (invalid attribute value) when diag infra lands.
                    SQL_ERROR
                }
            },
            _ => {
                // msodbcsql equivalent: index out of `NUMELEM(dwOptionsE)`
                // range, `SETRC_SERR_GOTO(retcode, RetExit)`.
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
    use crate::api::odbc_types::{SQL_HANDLE_DBC, SQL_HANDLE_ENV, SQL_NULL_HANDLE};

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
    fn set_env_attr_wrong_handle_type_invalid() {
        // Alloc an ENV, set its version, then alloc a DBC and pass DBC as the
        // env to SQLSetEnvAttr.
        let env = alloc_env();
        assert_eq!(
            set_attr(env, SQL_ATTR_ODBC_VERSION, SQL_OV_ODBC3_80),
            SQL_SUCCESS
        );
        let mut dbc: SqlHandle = ptr::null_mut();
        let ret = unsafe { sql_alloc_handle(SQL_HANDLE_DBC, env, &mut dbc) };
        assert_eq!(ret, SQL_SUCCESS);

        let ret = set_attr(dbc, SQL_ATTR_ODBC_VERSION, SQL_OV_ODBC3_80);
        assert_eq!(ret, SQL_INVALID_HANDLE);

        unsafe { sql_free_handle(SQL_HANDLE_DBC, dbc) };
        free_env(env);
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
    fn set_env_attr_after_free_returns_invalid() {
        // `free_handle` poisons `header.object_type` to `HandleType::Invalid`
        // before dropping the allocation. The tag check must catch the stale
        // pointer before any dereference of state.
        let env = alloc_env();
        free_env(env);
        let ret = set_attr(env, SQL_ATTR_ODBC_VERSION, SQL_OV_ODBC3_80);
        assert_eq!(ret, SQL_INVALID_HANDLE);
    }
}
