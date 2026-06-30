// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Built-in Entra ID (Azure AD) token factories backed by the Azure SDK for
//! Rust (`azure_identity`).
//!
//! The ODBC layer selects an authentication method (e.g.
//! `Authentication=ActiveDirectoryManagedIdentity`); mssql-tds owns token
//! acquisition. [`register_builtin_entra_factories`](ClientContext::register_builtin_entra_factories)
//! maps the method plus credential inputs onto an [`AzureIdentityTokenFactory`]
//! and installs it in the context's `auth_method_map` — unless the caller has
//! already injected their own factory for that method, which always wins.
//!
//! Only the token-credential flows that `azure_identity` actually ships are
//! handled here: service principal (client secret), managed identity, the
//! default credential chain, and workload identity. Username/password
//! (`ActiveDirectoryPassword`), interactive, and device-code flows have no
//! `azure_identity` credential and are handled elsewhere.

pub mod builder;
pub mod encoding;
pub mod factory;

pub use factory::AzureIdentityTokenFactory;
