// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TDS token types, parsers, and related data structures.

/// Result-set sensitivity classification (`DATACLASSIFICATION`, token `0xA3`).
pub mod data_classification;
pub(crate) mod fed_auth_info;
pub(crate) mod login_ack;
pub(crate) mod parsers;
/// Return value status and other token item types.
pub mod tokenitems;
/// Token type definitions, collation, and environment change types.
pub mod tokens;
