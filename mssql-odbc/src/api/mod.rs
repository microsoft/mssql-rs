// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod alloc_handle;
mod free_handle;
mod get_diag_rec;
pub(crate) mod odbc_types;
mod set_env_attr;
pub(crate) mod sqlstate;

// Exported ODBC entry points — the driver's public API surface.
// All `#[unsafe(no_mangle)] pub extern "C"` symbols are defined here.
mod exports;
pub use exports::*;
