// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLSetEnvAttr.
//!
//! Mirrors msodbcsql's `SQLSetEnvAttr`, replacing its internal options table
//! with typed fields on `EnvState`. The Driver Manager gates
//! `SQLAllocHandle(SQL_HANDLE_DBC)` on the version *it* recorded, not on the
//! driver's; because rejecting `SQL_OV_ODBC2` here leaves this driver's
//! environment `Unset` while the DM's is `2`, `alloc_handle::alloc_dbc`
//! enforces `HY010` independently. See registry entry 14.

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

/// # Safety
/// `environment_handle` must be null or point to a live `EnvHandle`.
/// `value_ptr` must contain the ODBC tagged integer value for `attribute`.
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

    match attribute {
        SQL_ATTR_ODBC_VERSION => {
            // ODBC tagged-pointer: integer values arrive as
            // `(SQLPOINTER)(uintptr_t)value`. Narrow with `try_into` rather
            // than `as`: on a 64-bit target a truncating cast would let a
            // value whose high half is set — say `0x1_0000_0003` — arrive as a
            // legitimate `SQL_OV_ODBC3`. No Driver Manager produces that, but
            // §2.2 and registry entry 14 state that every value other than the
            // two supported versions is `HY024`, and a truncating cast would
            // not quite deliver it.
            let raw = value_ptr as usize;
            let version = u32::try_from(raw)
                .ok()
                .and_then(|v| OdbcVersion::try_from(v).ok());
            match version {
                Some(v) => {
                    state.odbc_version = v;
                    SQL_SUCCESS
                }
                None => {
                    error!(raw, "SQLSetEnvAttr: invalid ODBC_VERSION value");
                    post_diag(&mut state, ERR_INVALID_ATTRIBUTE_VALUE);
                    SQL_ERROR
                }
            }
        }
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
    use crate::handles::handle_from_raw;
    use crate::test_support::TestHandles;

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

    /// The exported path narrows the tagged pointer to `u32` before matching a
    /// version, so a 64-bit value whose low half looks supported must still be
    /// rejected — otherwise §2.2's "every other value is `HY024`" would not
    /// hold on the export, only on `OdbcVersion::try_from`.
    #[cfg(target_pointer_width = "64")]
    #[test]
    fn set_odbc_version_rejects_a_value_wider_than_32_bits() {
        let h = TestHandles::with_unset_env();
        assert_eq!(
            set_attr(h.env, SQL_ATTR_ODBC_VERSION, SQL_OV_ODBC3_80),
            SQL_SUCCESS
        );

        let tagged = 0x1_0000_0000usize | SQL_OV_ODBC3 as usize;
        let ret = unsafe {
            crate::api::exports::SQLSetEnvAttr(
                h.env,
                SQL_ATTR_ODBC_VERSION,
                tagged as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_ERROR, "the low half must not be read in isolation");

        let env_ref = unsafe { handle_from_raw::<EnvHandle>(h.env) };
        assert_eq!(
            env_ref.inner.lock().unwrap().odbc_version,
            OdbcVersion::Odbc3_80,
            "a rejected value must leave the prior version intact"
        );
    }

    #[test]
    fn set_odbc_version_2_is_rejected() {
        let h = TestHandles::with_unset_env();
        assert_eq!(
            set_attr(h.env, SQL_ATTR_ODBC_VERSION, SQL_OV_ODBC3_80),
            SQL_SUCCESS
        );
        let ret = unsafe {
            crate::api::exports::SQLSetEnvAttr(
                h.env,
                SQL_ATTR_ODBC_VERSION,
                SQL_OV_ODBC2 as usize as SqlPointer,
                0,
            )
        };
        assert_eq!(ret, SQL_ERROR);
        let env_ref = unsafe { handle_from_raw::<EnvHandle>(h.env) };
        let state = env_ref.inner.lock().unwrap();
        assert_eq!(state.odbc_version, OdbcVersion::Odbc3_80);
        assert_eq!(state.diag_records.len(), 1);
        assert_eq!(&state.diag_records[0].sql_state, b"HY024");
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
            set_attr(env, SQL_ATTR_ODBC_VERSION, SQL_OV_ODBC3),
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
