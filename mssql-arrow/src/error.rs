// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use arrow_schema::DataType;
use mssql_tds::datatypes::bulk_copy_metadata::SqlDbType;

/// Errors specific to Arrow ↔ TDS operations.
#[derive(Debug, thiserror::Error)]
pub enum ArrowError {
    #[error("unsupported Arrow type `{arrow_type}` for column `{column_name}`")]
    UnsupportedArrowType {
        column_name: String,
        arrow_type: DataType,
    },

    #[error("type mismatch for column `{column_name}`: Arrow `{arrow_type}` → SQL `{sql_type:?}`")]
    TypeMismatch {
        column_name: String,
        arrow_type: DataType,
        sql_type: SqlDbType,
    },

    #[error("column count mismatch: Arrow has {arrow_columns}, destination has {sql_columns}")]
    ColumnCountMismatch {
        arrow_columns: usize,
        sql_columns: usize,
    },

    #[error(
        "decimal precision {precision} exceeds SQL Server maximum (38) for column `{column_name}`"
    )]
    PrecisionOverflow { column_name: String, precision: u8 },

    #[error(
        "string truncation for column `{column_name}`: value is {value_len} bytes, max is {max_len}"
    )]
    StringTruncation {
        column_name: String,
        value_len: usize,
        max_len: usize,
    },

    #[error("RecordBatch has zero columns")]
    EmptySchema,

    #[error("Arrow error: {0}")]
    ArrowError(#[from] arrow_schema::ArrowError),
}

impl From<ArrowError> for mssql_tds::error::Error {
    fn from(e: ArrowError) -> Self {
        mssql_tds::error::Error::TypeConversionError(e.to_string())
    }
}
