// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

mod alloc_handle;
mod free_handle;
pub(crate) mod odbc_types;
mod set_env_attr;

// Exported ODBC entry points — the driver's public API surface.
// All `#[unsafe(no_mangle)] pub extern "C"` symbols are defined here.
mod exports;
pub use exports::*;
