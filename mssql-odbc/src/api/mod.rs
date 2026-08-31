// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

pub(crate) mod alloc_handle;
mod attributes;
mod bind_col;
mod bind_param;
mod cancel;
mod catalog;
mod close_cursor;
mod col_attribute;
mod connect;
mod current_catalog;
mod describe_col;
mod describe_param;
mod disconnect;
mod driver_connect;
mod end_tran;
mod exec_common;
mod exec_direct;
mod execute;
pub(crate) mod fetch;
pub(crate) mod fetch_scroll;
pub(crate) mod free_handle;
pub(crate) mod get_connect_attr;
mod get_data;
mod get_desc_field;
mod get_desc_rec;
mod get_diag;
mod get_env_attr;
mod get_functions;
mod get_info;
mod get_type_info;
mod ird;
mod more_results;
mod num_result_cols;
pub(crate) mod odbc_types;
mod param_data;
mod prepare;
mod put_data;
mod row_count;
pub(crate) mod set_connect_attr;
pub(crate) mod set_desc_field;
mod set_desc_rec;
pub(crate) mod set_env_attr;
pub(crate) mod set_stmt_attr;
pub(crate) mod sqlstate;
mod txn;
pub(crate) mod type_rules;
pub(crate) mod util;

// Exported ODBC entry points — the driver's public API surface.
// All `#[unsafe(no_mangle)] pub extern "C"` symbols are defined here.
mod exports;
pub use exports::*;
