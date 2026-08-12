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
                OdbcVersion::Odbc2 => crate::api::odbc_types::SQL_OV_ODBC2,
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
