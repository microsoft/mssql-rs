// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Result set reading and row iteration.
//!
//! This module contains the types used to describe query output:
//!
//! - [`metadata::ColumnMetadata`] — per-column type and flag information
//!   returned with every result set.
//! - [`result::ReturnValue`] — output parameters and return-status values
//!   from stored procedures.
//!
//! Row-level iteration is driven by the [`ResultSet`](crate::ResultSet) trait
//! implemented on [`TdsClient`](crate::TdsClient).

/// Per-column type and flag metadata from COLMETADATA tokens.
pub mod metadata;
/// Output parameters and return status from stored procedures.
pub mod result;
