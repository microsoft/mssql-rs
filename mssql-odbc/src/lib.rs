// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::os::raw::c_short;

// ODBC handle types
const SQL_HANDLE_ENV: c_short = 1;
const SQL_HANDLE_DBC: c_short = 2;
const SQL_HANDLE_STMT: c_short = 3;
const SQL_HANDLE_DESC: c_short = 4;

// ODBC return codes
const SQL_SUCCESS: c_short = 0;
const SQL_ERROR: c_short = -1;

type SqlHandle = *mut std::ffi::c_void;
type SqlSmallInt = c_short;
type SqlReturn = SqlSmallInt;

/// Allocates an environment, connection, statement, or descriptor handle.
///
/// # Safety
/// Called from C/C++ via the ODBC Driver Manager. `output_handle` must be a valid pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLAllocHandle(
    handle_type: SqlSmallInt,
    input_handle: SqlHandle,
    output_handle: *mut SqlHandle,
) -> SqlReturn {
    let _ = (handle_type, input_handle);

    if output_handle.is_null() {
        return SQL_ERROR;
    }

    match handle_type {
        SQL_HANDLE_ENV | SQL_HANDLE_DBC | SQL_HANDLE_STMT | SQL_HANDLE_DESC => {
            // TODO: allocate real handle objects
            SQL_SUCCESS
        }
        _ => SQL_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ptr;

    #[test]
    fn alloc_env_handle_succeeds() {
        let mut handle: SqlHandle = ptr::null_mut();
        let ret = unsafe { SQLAllocHandle(SQL_HANDLE_ENV, ptr::null_mut(), &mut handle) };
        assert_eq!(ret, SQL_SUCCESS);
    }

    #[test]
    fn alloc_invalid_handle_type_fails() {
        let mut handle: SqlHandle = ptr::null_mut();
        let ret = unsafe { SQLAllocHandle(99, ptr::null_mut(), &mut handle) };
        assert_eq!(ret, SQL_ERROR);
    }

    #[test]
    fn alloc_null_output_handle_fails() {
        let ret = unsafe { SQLAllocHandle(SQL_HANDLE_ENV, ptr::null_mut(), ptr::null_mut()) };
        assert_eq!(ret, SQL_ERROR);
    }
}
