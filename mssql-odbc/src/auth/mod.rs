// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Authentication wiring for mssql-odbc.
//!
//! [`entra`] resolves a connection's authentication method onto a token factory
//! or set of credentials. `interactive` implements Entra interactive sign-in and
//! is compiled only on Windows, matching msodbcsql, whose Unix build omits the
//! equivalent translation unit. `integrated` implements
//! `ActiveDirectoryIntegrated` on every platform: through `msqa`
//! (`mssql-auth.dll`) on Windows and through `federated` (Kerberos to ADFS
//! WS-Trust) elsewhere. `federated` is also compiled into Windows test builds
//! so its protocol handling is covered on every CI leg. This module only
//! re-exports the connect-flow entry point.

mod entra;
#[cfg(any(not(windows), test))]
mod federated;
mod integrated;
#[cfg(windows)]
mod interactive;
#[cfg(windows)]
mod msqa;

pub(crate) use entra::{UnsupportedAuth, configure_auth};
