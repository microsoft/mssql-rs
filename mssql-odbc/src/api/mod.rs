// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod alloc_handle;
mod close_cursor;
mod disconnect;
mod driver_connect;
mod exec_direct;
pub(crate) mod fetch;
mod free_handle;
mod get_data;
mod get_diag;
pub(crate) mod odbc_types;
mod set_env_attr;
pub(crate) mod sqlstate;
pub(crate) mod util;

// Exported ODBC entry points — the driver's public API surface.
// All `#[unsafe(no_mangle)] pub extern "C"` symbols are defined here.
mod exports;
pub use exports::*;
