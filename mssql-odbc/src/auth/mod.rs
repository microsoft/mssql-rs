// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Authentication wiring for mssql-odbc.
//!
//! [`entra`] resolves a connection's authentication method onto a token factory
//! or set of credentials. `interactive` and `msqa` implement Entra interactive
//! sign-in and are compiled only on Windows, matching msodbcsql, whose Unix
//! build omits the equivalent translation unit. This module only re-exports the
//! connect-flow entry point.

mod entra;
#[cfg(windows)]
mod interactive;
#[cfg(windows)]
mod msqa;

pub(crate) use entra::configure_auth;
