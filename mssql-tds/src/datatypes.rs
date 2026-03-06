// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

#[cfg(test)]
// Not public API — experimental Arrow integration retained for testing only.
mod arrow_writer;
pub mod bulk_copy_metadata;
pub mod column_values;
pub mod decoder;
pub(crate) mod encoder;
pub mod lcid_encoding;
pub mod row_writer;
pub mod sql_json;
pub mod sql_string;
pub mod sql_vector;
pub mod sqldatatypes;
pub mod sqltypes;
pub mod tds_value_serializer;
