// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Arrow-native SQL Server access. See the crate README for options and type mappings.

mod driver;
mod error;
pub mod options;
mod runtime;
mod statement;
mod types;

pub use adbc_core;
pub use driver::{MssqlConnection, MssqlDatabase, MssqlDriver};
pub use statement::MssqlStatement;

#[cfg(feature = "ffi")]
adbc_ffi::export_driver!(AdbcDriverMssqlInit, MssqlDriver);
