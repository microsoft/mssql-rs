// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Authentication wiring for mssql-odbc.
//!
//! The implementation lives in [`entra`]; this module only re-exports the
//! connect-flow entry point.

mod entra;

pub(crate) use entra::configure_auth;
