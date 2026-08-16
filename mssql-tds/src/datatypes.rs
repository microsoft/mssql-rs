// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! TDS data type definitions and serialization.
//!
//! Provides Rust representations of SQL Server data types, encoding/decoding
//! between TDS wire format and Rust values, and metadata structures for
//! bulk copy operations.

/// Metadata types for bulk copy column mappings.
pub mod bulk_copy_metadata;
/// Decoded column value types returned in result rows.
pub mod column_values;
/// Decimal/numeric decoding helpers.
pub mod decoder;
pub(crate) mod encoder;
pub(crate) mod lcid_encoding;
/// Trait for pluggable row decoding sinks.
pub mod row_writer;
/// Timing harness for the `RowWriter` value-sink path. Every bench in it is
/// `#[ignore]`d, so the normal suite only runs its self-check.
#[cfg(test)]
mod sink_bench;
/// SQL Server `json` column type.
pub mod sql_json;
/// SQL Server character string type with encoding.
pub mod sql_string;
/// Table-valued parameter (TVP) types for RPC calls.
pub mod sql_tvp;
/// SQL Server `vector` column type.
pub mod sql_vector;
/// Wire-level TDS data type identifiers.
pub mod sqldatatypes;
/// Input parameter types for RPC calls.
pub mod sqltypes;
pub(crate) mod tds_value_serializer;
