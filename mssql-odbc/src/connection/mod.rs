// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Connection handling for the ODBC driver.
//!
//! - [`connection_string_parser`]: parses the `SQLDriverConnect` connection
//!   string into [`ConnectionParams`](connection_string_parser::ConnectionParams),
//!   mirroring msodbcsql's `ParseAttrStr`.
//! - `odbc_authentication_validator` / `odbc_authentication_transformer`:
//!   resolve the parsed `Authentication` keyword into a concrete auth method.

pub(crate) mod connection_string_parser;
pub(crate) mod odbc_authentication_transformer;
pub(crate) mod odbc_authentication_validator;
mod odbc_supported_auth_keywords;

pub(crate) use connection_string_parser::parse_connection_string;
