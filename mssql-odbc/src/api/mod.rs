// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub(crate) mod alloc_handle;
mod close_cursor;
mod describe_col;
mod disconnect;
mod driver_connect;
mod exec_direct;
pub(crate) mod fetch;
pub(crate) mod free_handle;
mod get_data;
mod get_diag;
mod more_results;
mod num_result_cols;
pub(crate) mod odbc_types;
mod prepare;
pub(crate) mod set_connect_attr;
pub(crate) mod set_env_attr;
pub(crate) mod sqlstate;
pub(crate) mod util;

// Exported ODBC entry points — the driver's public API surface.
// All `#[unsafe(no_mangle)] pub extern "C"` symbols are defined here.
mod exports;
pub use exports::*;
