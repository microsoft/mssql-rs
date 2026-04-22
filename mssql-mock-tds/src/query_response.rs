// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Query response definitions for the mock TDS server

use bytes::{BufMut, BytesMut};
use std::collections::HashMap;

/// SQL data types supported by the mock server
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlDataType {
    /// TinyInt - 1 byte integer (0-255)
    TinyInt,
    /// SmallInt - 2 byte integer
    SmallInt,
    /// Int - 4 byte integer
    Int,
    /// BigInt - 8 byte integer
    BigInt,
    /// NVarChar - UTF16 string
    NVarChar,
}

impl SqlDataType {
    /// Get the TDS type code for this data type
    pub fn tds_type_code(&self) -> u8 {
        match self {
            SqlDataType::TinyInt => 0x26,  // IntN with length 1
            SqlDataType::SmallInt => 0x26, // IntN with length 2
            SqlDataType::Int => 0x26,      // IntN with length 4
            SqlDataType::BigInt => 0x26,   // IntN with length 8
            SqlDataType::NVarChar => 0xE7, // NVarCharType
        }
    }

    /// Get the length byte for this data type (used in ColMetadata)
    pub fn max_length(&self) -> u8 {
        match self {
            SqlDataType::TinyInt => 1,
            SqlDataType::SmallInt => 2,
            SqlDataType::Int => 4,
            SqlDataType::BigInt => 8,
            SqlDataType::NVarChar => 255, // Handled specially
        }
    }
}

/// A column value that can be serialized
#[derive(Debug, Clone)]
pub enum ColumnValue {
    TinyInt(u8),
    SmallInt(i16),
    Int(i32),
    BigInt(i64),
    NVarChar(String),
    Null,
}

impl ColumnValue {
    /// Get the SQL data type for this value
    pub fn data_type(&self) -> SqlDataType {
        match self {
            ColumnValue::TinyInt(_) => SqlDataType::TinyInt,
            ColumnValue::SmallInt(_) => SqlDataType::SmallInt,
            ColumnValue::Int(_) => SqlDataType::Int,
            ColumnValue::BigInt(_) => SqlDataType::BigInt,
            ColumnValue::NVarChar(_) => SqlDataType::NVarChar,
            ColumnValue::Null => SqlDataType::Int, // Default to Int for NULL
        }
    }

    /// Write this value to a buffer for a Row token
    pub fn write_to_buffer(&self, buf: &mut BytesMut) {
        match self {
            ColumnValue::TinyInt(v) => {
                buf.put_u8(1); // Length indicator
                buf.put_u8(*v);
            }
            ColumnValue::SmallInt(v) => {
                buf.put_u8(2); // Length indicator
                buf.put_i16_le(*v);
            }
            ColumnValue::Int(v) => {
                buf.put_u8(4); // Length indicator
                buf.put_i32_le(*v);
            }
            ColumnValue::BigInt(v) => {
                buf.put_u8(8); // Length indicator
                buf.put_i64_le(*v);
            }
            ColumnValue::NVarChar(v) => {
                let utf16_bytes: Vec<u8> = v
                    .encode_utf16()
                    .flat_map(|ch| ch.to_le_bytes().into_iter())
                    .collect();
                buf.put_u16_le(utf16_bytes.len() as u16);
                buf.put_slice(&utf16_bytes);
            }
            ColumnValue::Null => {
                buf.put_u8(0); // Length 0 means NULL for IntN
            }
        }
    }
}

/// A column definition in a result set
#[derive(Debug, Clone)]
pub struct ColumnDefinition {
    pub name: String,
    pub data_type: SqlDataType,
}

impl ColumnDefinition {
    /// Create a new column definition
    pub fn new(name: impl Into<String>, data_type: SqlDataType) -> Self {
        Self {
            name: name.into(),
            data_type,
        }
    }
}

/// A row of data
#[derive(Debug, Clone)]
pub struct Row {
    pub values: Vec<ColumnValue>,
}

impl Row {
    /// Create a new row
    pub fn new(values: Vec<ColumnValue>) -> Self {
        Self { values }
    }
}

/// A complete query response definition
#[derive(Debug, Clone)]
pub struct QueryResponse {
    pub columns: Vec<ColumnDefinition>,
    pub rows: Vec<Row>,
}

impl QueryResponse {
    /// Create a new query response
    pub fn new(columns: Vec<ColumnDefinition>, rows: Vec<Row>) -> Self {
        Self { columns, rows }
    }

    /// Helper to create a response for SELECT 1
    pub fn select_one() -> Self {
        Self {
            columns: vec![ColumnDefinition::new("", SqlDataType::Int)],
            rows: vec![Row::new(vec![ColumnValue::Int(1)])],
        }
    }

    /// Helper to create a response for SELECT CAST(1 AS BIGINT), 2, 3
    pub fn select_multiple_types() -> Self {
        Self {
            columns: vec![
                ColumnDefinition::new("", SqlDataType::BigInt),
                ColumnDefinition::new("", SqlDataType::Int),
                ColumnDefinition::new("", SqlDataType::Int),
            ],
            rows: vec![Row::new(vec![
                ColumnValue::BigInt(1),
                ColumnValue::Int(2),
                ColumnValue::Int(3),
            ])],
        }
    }
}

/// Registry of query responses
pub struct QueryRegistry {
    responses: HashMap<String, QueryResponse>,
}

impl QueryRegistry {
    /// Create a new query registry with default responses
    pub fn new() -> Self {
        let mut registry = Self {
            responses: HashMap::new(),
        };

        // Add default responses
        registry.register("SELECT 1", QueryResponse::select_one());
        registry.register(
            "SELECT CAST(1 AS BIGINT), 2, 3",
            QueryResponse::select_multiple_types(),
        );

        registry
    }

    /// Register a query response
    pub fn register(&mut self, query: impl Into<String>, response: QueryResponse) {
        let query = query.into().to_uppercase();
        self.responses.insert(query, response);
    }

    /// Get a response for a query
    pub fn get(&self, query: &str) -> Option<&QueryResponse> {
        self.responses.get(&query.to_uppercase())
    }
}

impl Default for QueryRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sql_data_types() {
        assert_eq!(SqlDataType::TinyInt.tds_type_code(), 0x26);
        assert_eq!(SqlDataType::SmallInt.tds_type_code(), 0x26);
        assert_eq!(SqlDataType::Int.tds_type_code(), 0x26);
        assert_eq!(SqlDataType::BigInt.tds_type_code(), 0x26);
        assert_eq!(SqlDataType::NVarChar.tds_type_code(), 0xE7);

        assert_eq!(SqlDataType::TinyInt.max_length(), 1);
        assert_eq!(SqlDataType::SmallInt.max_length(), 2);
        assert_eq!(SqlDataType::Int.max_length(), 4);
        assert_eq!(SqlDataType::BigInt.max_length(), 8);
        assert_eq!(SqlDataType::NVarChar.max_length(), 255);
    }

    #[test]
    fn test_column_value_write() {
        let mut buf = bytes::BytesMut::new();

        let null_val = ColumnValue::Null;
        assert_eq!(null_val.data_type(), SqlDataType::Int);
        null_val.write_to_buffer(&mut buf);
        assert_eq!(&buf[..], &[0]);
        buf.clear();

        let nvarchar_val = ColumnValue::NVarChar("test".to_string());
        assert_eq!(nvarchar_val.data_type(), SqlDataType::NVarChar);
        nvarchar_val.write_to_buffer(&mut buf);
        // length is 4 u16 chars => 8 bytes, so 0x08 0x00 is the length followed by the utf16 LE bytes
        assert_eq!(&buf[0..2], &[8, 0]);
        buf.clear();

        let int_val = ColumnValue::Int(1);
        assert_eq!(int_val.data_type(), SqlDataType::Int);
        int_val.write_to_buffer(&mut buf);
        assert_eq!(&buf[..], &[4, 1, 0, 0, 0]);
    }

    #[test]
    fn test_select_nvarchar_type() {
        let mut registry = QueryRegistry::new();
        registry.register(
            "SELECT 'test'",
            QueryResponse::new(
                vec![ColumnDefinition::new("", SqlDataType::NVarChar)],
                vec![Row::new(vec![ColumnValue::NVarChar("test".to_string())])],
            ),
        );
        let resp = registry.get("SELECT 'test'").unwrap();
        assert_eq!(resp.columns.len(), 1);
        assert_eq!(resp.rows.len(), 1);
    }
}
