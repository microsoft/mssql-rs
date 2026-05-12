// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Exported ODBC entry points for the msodbcsql18 shared library.
//!
//! Every `#[unsafe(no_mangle)] pub extern "C"` function that appears in the
//! driver's symbol table is listed here. Implementations live in sibling
//! modules (e.g. `alloc_handle.rs`) as `pub(crate)` functions.
//!
//! This file acts as the driver's export manifest — the Rust equivalent of a
//! Windows `.def` file or a C header listing the public API surface.

use super::odbc_types::{SqlHandle, SqlReturn, SqlSmallInt};

// ---- Handle allocation and management ---------------------------------------

/// See [`alloc_handle::sql_alloc_handle`] for full safety requirements.
///
/// # Safety
/// Called from C via the ODBC Driver Manager. `output_handle` must be a valid,
/// aligned, writable pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLAllocHandle(
    handle_type: SqlSmallInt,
    input_handle: SqlHandle,
    output_handle: *mut SqlHandle,
) -> SqlReturn {
    unsafe { super::alloc_handle::sql_alloc_handle(handle_type, input_handle, output_handle) }
}

/// See [`free_handle::sql_free_handle`] for full safety requirements.
///
/// # Safety
/// Called from C via the ODBC Driver Manager. `handle` must have been
/// allocated by `SQLAllocHandle` and not already freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn SQLFreeHandle(handle_type: SqlSmallInt, handle: SqlHandle) -> SqlReturn {
    unsafe { super::free_handle::sql_free_handle(handle_type, handle) }
}
