// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Connection management types for TDS protocol communication with SQL Server.
//!
//! Key types:
//! - [`tds_client::TdsClient`] — primary client for executing queries and managing connections
//! - [`client_context::ClientContext`] — connection configuration (credentials, encryption, timeouts)
//! - [`bulk_copy::BulkCopy`] — bulk data loading

pub mod bulk_copy;
pub(crate) mod bulk_copy_state;
/// Client connection context and authentication factories.
pub mod client_context;
pub(crate) mod connection_actions;
/// Server cursor RPCs (`sp_cursor*`) via the
/// [`CursorClient`](crate::connection::cursor_ops::CursorClient) trait.
pub mod cursor_ops;
pub(crate) mod datasource_parser;
/// Built-in Entra ID (Azure AD) token factories backed by `azure_identity`.
///
/// Compiled for the `entra-auth` feature, and also under `cfg(test)` so the
/// no-network unit tests run in the default test profile (CI does not pass
/// `--all-features`). `azure_identity` is already a dev-dependency, so the
/// `test` arm adds no extra build cost.
#[cfg(any(feature = "entra-auth", test))]
pub mod entra_auth;
pub(crate) mod execution_context;
pub(crate) mod instance_cache;
pub(crate) mod metadata_retriever;
/// ODBC-style authentication keyword transform.
pub mod odbc_authentication_transformer;
/// ODBC-style authentication keyword validation.
pub mod odbc_authentication_validator;
pub(crate) mod odbc_supported_auth_keywords;
pub(crate) mod session_recovery;
/// Primary client type and result set traits.
pub mod tds_client;
/// Transport layer (TCP, Named Pipes, Shared Memory).
pub mod transport;
