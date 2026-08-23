// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! ODBC-style authentication keyword handling, owned by the Python binding.
//!
//! These modules were moved out of `mssql-tds` so each binding controls its
//! own supported authentication method set. `mssql-tds` only takes (or asks
//! for) a token for the federated-auth flows; the keyword vocabulary,
//! validation, and precedence rules live here.

pub mod odbc_authentication_transformer;
pub mod odbc_authentication_validator;
pub mod odbc_supported_auth_keywords;
