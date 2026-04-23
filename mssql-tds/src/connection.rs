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
pub(crate) mod datasource_parser;
pub(crate) mod execution_context;
pub(crate) mod instance_cache;
pub(crate) mod metadata_retriever;
/// ODBC-style authentication keyword transform.
pub mod odbc_authentication_transformer;
/// ODBC-style authentication keyword validation.
pub mod odbc_authentication_validator;
pub(crate) mod odbc_supported_auth_keywords;
/// Primary client type and result set traits.
pub mod tds_client;
/// Transport layer (TCP, Named Pipes, Shared Memory).
pub mod transport;
