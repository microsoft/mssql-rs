// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#![warn(missing_docs)]

//! Async Rust implementation of the TDS (Tabular Data Stream) protocol for SQL Server
//! and Azure SQL Database.
//!
//! # Overview
//!
//! `mssql-tds` provides a low-level, async client for communicating with SQL Server
//! using the TDS protocol. It handles connection negotiation (prelogin, TLS, login7),
//! query execution, result set streaming, bulk copy, RPC calls, and transaction
//! management.
//!
//! # Feature flags
//!
//! | Flag | Default | Description |
//! |------|---------|-------------|
//! | `integrated-auth` | **yes** | Enables both `sspi` and `gssapi` |
//! | `sspi` | via `integrated-auth` | Windows SSPI (Kerberos/NTLM) |
//! | `gssapi` | via `integrated-auth` | Unix GSSAPI (Kerberos) via runtime `dlopen` |
//! | `tls-schannel-direct-on-windows` | **yes** | Enables `tls-schannel-direct` |
//! | `tls-schannel-direct` | via `tls-schannel-direct-on-windows` | Windows: TLS through the in-tree Schannel engine instead of `native-tls`. No effect on other targets. |
//! | `tls-network-framework-on-macos` | **yes** | Enables `tls-network-framework` |
//! | `tls-network-framework` | via `tls-network-framework-on-macos` | macOS: Network.framework for Strict TCP connections, including TLS 1.3. No effect on other targets. |
//!
//! The Network.framework feature requires a macOS deployment target of 10.15 or
//! later. For x86_64 macOS builds, set `MACOSX_DEPLOYMENT_TARGET=10.15` or later
//! for the entire build. See the
//! [connection behavior guide](https://github.com/microsoft/mssql-rs/blob/main/mssql-tds/docs/connection-behavior.md#tls-on-macos).
//!
//! These examples target version 0.4. If it is not yet available on crates.io,
//! [use a source checkout](https://github.com/microsoft/mssql-rs/blob/main/mssql-tds/docs/client-guide.md#using-a-source-checkout).
//!
//! Disable the defaults to omit built-in integrated authentication and use
//! `native-tls` on every platform:
//!
//! ```toml
//! mssql-tds = { version = "0.4", default-features = false }
//! ```
//!
//! # Quick start
//!
//! Set `DB_USER` and `DB_PASSWORD`. The default Strict encryption mode requires
//! a TDS-8-capable server and a trusted certificate matching the hostname.
//! For TDS 7.4, set `context.encryption_options.mode` to
//! [`core::EncryptionSetting::On`] before connecting.
//!
//! ```rust,no_run
//! use mssql_tds::connection::client_context::ClientContext;
//! use mssql_tds::connection::tds_client::ResultSet;
//! use mssql_tds::connection_provider::tds_connection_provider::TdsConnectionProvider;
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let mut context = ClientContext::default();
//!     context.user_name = std::env::var("DB_USER")?;
//!     context.password = std::env::var("DB_PASSWORD")?;
//!     context.database = "master".into();
//!
//!     let provider = TdsConnectionProvider {};
//!     let mut client = provider
//!         .create_client(context, "tcp:localhost,1433", None)
//!         .await?;
//!
//!     client.execute("SELECT 1 AS value".into(), ()).await?;
//!
//!     if client.on_rows() {
//!         while let Some(row) = client.next_row().await? {
//!             println!("{row:?}");
//!         }
//!     }
//!
//!     client.close_query().await?;
//!     Ok(())
//! }
//! ```
//!
//! # Modules
//!
//! - [`connection`] — Client type ([`connection::tds_client::TdsClient`]),
//!   connection context, and authentication configuration.
//! - [`connection_provider`] — Connection factory
//!   ([`connection_provider::tds_connection_provider::TdsConnectionProvider`]).
//! - [`core`] — Shared types: [`core::TdsResult`], [`core::EncryptionOptions`],
//!   [`core::CancelHandle`].
//! - [`cursor`] — Cursor types, bitflags, and response structs for `sp_cursor*` RPCs.
//! - [`datatypes`] — SQL Server data types and column value representations.
//! - [`error`] — Error definitions.
//! - [`message`] — TDS message types (prelogin, login7, etc.).
//! - [`query`] — Query metadata and column descriptors.
//! - [`security`] — Integrated authentication (SSPI/GSSAPI) and Always Encrypted.
//! - [`token`] — TDS token stream parsing (COLMETADATA, ROW, DONE, etc.).

pub mod connection;
pub mod connection_provider;
/// Shared types: result aliases, encryption settings, and cancellation.
pub mod core;

// `EncodingType::encoding` and `lcid_to_encoding` hand out `&'static
// encoding_rs::Encoding`, so a consumer needs the exact same `encoding_rs` build
// to name the type. Re-exported so they inherit ours instead of guessing a
// matching version.
pub use encoding_rs;
/// Cursor types and response structures for TDS cursor RPCs.
pub mod cursor;
pub mod datatypes;
/// Error definitions for TDS operations.
pub mod error;
pub(crate) mod handler;
pub(crate) mod io;
pub mod message;
pub mod query;
pub mod security;
pub(crate) mod sql_identifier;
pub(crate) mod ssrp;
pub mod token;
#[doc(hidden)]
pub mod trace_context;

// Expose internal APIs for fuzzing
#[cfg(fuzzing)]
pub mod fuzz_support;

// Test-only helpers for driving a `TdsClient` from scripted TDS tokens. Gated
// behind `test-util` so downstream crates can unit-test client-driven paths.
#[cfg(any(test, feature = "test-util"))]
pub mod test_client_support;

// Test-only plumbing for feeding hand-built TDS packets to a real
// `NetworkTransport`. Spans `io` and `connection::transport`, so it belongs to
// neither.
#[cfg(test)]
pub(crate) mod test_packet_support;

// Compiles the README's code blocks under `cargo test --doc`.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
