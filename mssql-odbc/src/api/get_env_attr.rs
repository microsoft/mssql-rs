// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Implementation of SQLGetEnvAttr.

use tracing::{debug, error};

use crate::api::odbc_types::{
    SQL_ATTR_ODBC_VERSION, SQL_ERROR, SQL_INVALID_HANDLE, SQL_SUCCESS, SqlHandle, SqlInteger,
    SqlPointer, SqlReturn,
};
use crate::api::sqlstate::{ERR_INVALID_ATTRIBUTE_IDENTIFIER, post_diag};
use crate::api::util::write_if_some;
use crate::error::free_errors;
use crate::handles::{EnvHandle, HandleType, OdbcVersion, handle_from_raw};

/// Returns an environment attribute value.
///
/// # Safety
/// - `environment_handle` must be a valid ENV handle.
/// - `value_ptr` and `string_length_ptr` must satisfy ODBC output-pointer
///   requirements for the requested attribute.
pub(crate) unsafe fn sql_get_env_attr(
    environment_handle: SqlHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
    _buffer_length: SqlInteger,
    string_length_ptr: *mut SqlInteger,
) -> SqlReturn {
    debug!(
        ?environment_handle,
        attribute,
        ?value_ptr,
        ?string_length_ptr,
        "SQLGetEnvAttr called",
    );

    crate::ffi_entry!("SQLGetEnvAttr", unsafe {
        sql_get_env_attr_impl(environment_handle, attribute, value_ptr, string_length_ptr)
    })
}

/// # Safety
/// `environment_handle` must be null or point to a live `EnvHandle`.
/// `value_ptr`, when non-null, must be writable for one `u32`, and
/// `string_length_ptr`, when non-null, must be writable for one `SqlInteger`.
unsafe fn sql_get_env_attr_impl(
    environment_handle: SqlHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
    string_length_ptr: *mut SqlInteger,
) -> SqlReturn {
    if environment_handle.is_null() {
        error!("SQLGetEnvAttr: environment_handle is null");
        return SQL_INVALID_HANDLE;
    }

    let env = unsafe { handle_from_raw::<EnvHandle>(environment_handle) };
    debug_assert_eq!(
        env.object_type,
        HandleType::Env,
        "SQLGetEnvAttr: handle is not an ENV"
    );
    sql_get_env_attr_safe(env, attribute, value_ptr, string_length_ptr)
}

fn sql_get_env_attr_safe(
    env: &EnvHandle,
    attribute: SqlInteger,
    value_ptr: SqlPointer,
    string_length_ptr: *mut SqlInteger,
) -> SqlReturn {
    let Ok(mut state) = env.inner.lock() else {
        error!("SQLGetEnvAttr: env mutex poisoned");
        return SQL_ERROR;
    };
    free_errors(&mut state);

    match attribute {
        SQL_ATTR_ODBC_VERSION => {
            let v = match state.odbc_version {
                OdbcVersion::Unset => 0u32,
                OdbcVersion::Odbc3 => crate::api::odbc_types::SQL_OV_ODBC3,
                OdbcVersion::Odbc3_80 => crate::api::odbc_types::SQL_OV_ODBC3_80,
            };
            unsafe { write_if_some(value_ptr as *mut u32, v) };
            unsafe { write_if_some(string_length_ptr, std::mem::size_of::<u32>() as i32) };
            SQL_SUCCESS
        }
        _ => {
            error!(attribute, "SQLGetEnvAttr: unsupported env attribute");
            post_diag(&mut state, ERR_INVALID_ATTRIBUTE_IDENTIFIER);
            SQL_ERROR
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ptr;

    use super::*;
    use crate::api::odbc_types::{SQL_OV_ODBC2, SQL_OV_ODBC3, SQL_OV_ODBC3_80};
    use crate::test_support::TestHandles;

    #[test]
    fn exported_set_and_get_round_trip_supported_odbc_3_versions() {
        let h = TestHandles::with_unset_env();
        for version in [SQL_OV_ODBC3, SQL_OV_ODBC3_80] {
            assert_eq!(
                unsafe {
                    crate::api::exports::SQLSetEnvAttr(
                        h.env,
                        SQL_ATTR_ODBC_VERSION,
                        version as usize as SqlPointer,
                        0,
                    )
                },
                SQL_SUCCESS
            );

            let mut actual = 0u32;
            let mut length = 0;
            assert_eq!(
                unsafe {
                    crate::api::exports::SQLGetEnvAttr(
                        h.env,
                        SQL_ATTR_ODBC_VERSION,
                        ptr::from_mut(&mut actual).cast(),
                        std::mem::size_of::<u32>() as SqlInteger,
                        &mut length,
                    )
                },
                SQL_SUCCESS
            );
            assert_eq!(actual, version);
            assert_eq!(length, std::mem::size_of::<u32>() as SqlInteger);
        }
    }

    #[test]
    fn rejected_odbc_2_does_not_change_the_getter_result() {
        let h = TestHandles::with_unset_env();
        assert_eq!(
            unsafe {
                crate::api::exports::SQLSetEnvAttr(
                    h.env,
                    SQL_ATTR_ODBC_VERSION,
                    SQL_OV_ODBC3 as usize as SqlPointer,
                    0,
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(
            unsafe {
                crate::api::exports::SQLSetEnvAttr(
                    h.env,
                    SQL_ATTR_ODBC_VERSION,
                    SQL_OV_ODBC2 as usize as SqlPointer,
                    0,
                )
            },
            SQL_ERROR
        );

        let mut actual = 0u32;
        assert_eq!(
            unsafe {
                crate::api::exports::SQLGetEnvAttr(
                    h.env,
                    SQL_ATTR_ODBC_VERSION,
                    ptr::from_mut(&mut actual).cast(),
                    std::mem::size_of::<u32>() as SqlInteger,
                    ptr::null_mut(),
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(actual, SQL_OV_ODBC3);
    }

    #[test]
    fn unset_version_reads_as_zero() {
        let h = TestHandles::with_unset_env();
        let mut actual = u32::MAX;
        assert_eq!(
            unsafe {
                sql_get_env_attr(
                    h.env,
                    SQL_ATTR_ODBC_VERSION,
                    ptr::from_mut(&mut actual).cast(),
                    std::mem::size_of::<u32>() as SqlInteger,
                    ptr::null_mut(),
                )
            },
            SQL_SUCCESS
        );
        assert_eq!(actual, 0);
    }
}
