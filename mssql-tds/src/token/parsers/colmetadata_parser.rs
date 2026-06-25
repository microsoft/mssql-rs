// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! # COLMETADATA Token Parser
//!
//! Parses COLMETADATA tokens (0x81) which describe the structure of result set columns.
//! This token appears before any ROW tokens and defines the schema of the data that follows.
//!
//! ## Token Byte Layout
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────────────────┐
//! │                  COLMETADATA Token (variable length)                    │
//! ├────────────┬────────────────────────────────────────────────────────────┤
//! │ ColCount   │              Column Definitions (repeated)                 │
//! │ (2 bytes)  │                  (ColCount times)                          │
//! │  UINT16    │                                                            │
//! └────────────┴────────────────────────────────────────────────────────────┘
//!     0-1                        2 ... N
//!
//! Special case: ColCount = 0xFFFF means "no metadata" (e.g., for INSERT/UPDATE)
//!
//! Per-Column Structure:
//! ┌────────────────────────────────────────────────────────────────────────┐
//! │                      Column Definition                                 │
//! ├───────────┬───────┬──────────┬─────────────┬──────────────┬────────────┤
//! │ UserType  │ Flags │ DataType │  TypeInfo   │   TableName  │ ColumnName │
//! │ (4 bytes) │(2 byte│ (1 byte) │  (variable) │  (optional)  │  (variable)│
//! │  UINT32   │ UINT16│   BYTE   │             │ B_VARCHAR(1) │B_VARCHAR(1)│
//! └───────────┴───────┴──────────┴─────────────┴──────────────┴────────────┘
//!     0-3       4-5       6         7 ... M      M+1 ... P     P+1 ... Q
//!
//! Flags (bitmask, per MS-TDS COLMETADATA, least-significant-bit order):
//!   0x0001 = Nullable (fNullable)
//!   0x0002 = Case-sensitive collation (fCaseSen)
//!   0x000C = Updateable (2-bit: 0=read-only, 1=read/write, 2=unknown)
//!   0x0010 = Identity column (fIdentity)
//!   0x0020 = Computed column (fComputed)
//!   0x00C0 = usReservedODBC (2-bit, reserved)
//!   0x0100 = Fixed-length CLR type (fFixedLenCLRType)
//!   0x0200 = Default value, used for table-valued parameters (fIsDefault)
//!   0x0400 = Sparse column set (fSparseColumnSet)
//!   0x0800 = Encrypted, Always Encrypted (fEncrypted)
//!   0x2000 = Hidden, e.g. FOR BROWSE (fHidden)
//!   0x4000 = Key column, used in cursor operations (fKey)
//!   0x8000 = Nullable unknown (fNullableUnknown)
//!
//! TypeInfo varies by DataType:
//!   - INT/BIGINT:    No additional info
//!   - VARCHAR(n):    MaxLength(2 bytes) + Collation(5 bytes)
//!   - DECIMAL(p,s):  MaxLength(1 byte) + Precision(1) + Scale(1)
//!   - (See read_type_info for full details)
//!
//! TableName appears only for TEXT/NTEXT/IMAGE types:
//!   - Multi-part table name (server.database.schema.table)
//!   - Specifies source table for LOB columns
//! ```
//!
//! ## Example
//!
//! ```text
//! // For query: SELECT Id, Name, Age FROM Users
//! // COLMETADATA token contains:
//! //   ColCount = 3
//! //   Column 1: UserType=0, Flags=0x00 (not nullable), DataType=INT, Name="Id"
//! //   Column 2: UserType=0, Flags=0x09 (nullable, updatable), DataType=NVARCHAR(50), Name="Name"
//! //   Column 3: UserType=0, Flags=0x01 (nullable), DataType=INT, Name="Age"
//! ```
//!
//! ## Token Flow
//!
//! ```text
//! Query: SELECT * FROM Users
//!
//! Server response:
//!   1. COLMETADATA ← Defines columns (this parser)
//!   2. ROW         ← First data row
//!   3. ROW         ← Second data row
//!   4. ...
//!   5. DONE        ← End of result set
//! ```

use std::io::Error;

use async_trait::async_trait;

use super::super::tokens::Tokens;
use super::common::TokenParser;
use crate::{core::TdsResult, io::packet_reader::TdsPacketReader};
use crate::{
    datatypes::sqldatatypes::{TdsDataType, read_type_info},
    io::token_stream::ParserContext,
    query::metadata::{ColumnMetadata, MultiPartName},
    token::tokens::ColMetadataToken,
};

/// Parser for COLMETADATA token (0x81) - defines result set column schema
#[derive(Default)]
pub(crate) struct ColMetadataTokenParser {
    // Do we want to create a new parser for every connection, or should
    // this value be passed as a context to the parser? Likely SessionSettings?
    pub is_column_encryption_supported: bool,
}

#[async_trait]
impl<T> TokenParser<T> for ColMetadataTokenParser
where
    T: TdsPacketReader + Send + Sync,
{
    async fn parse(&self, reader: &mut T, _context: &ParserContext) -> TdsResult<Tokens> {
        // Read column count (2 bytes)
        // Special value 0xFFFF indicates no metadata (used for INSERT/UPDATE/DELETE)
        let col_count = reader.read_uint16().await?;

        if self.is_column_encryption_supported {
            return Err(crate::error::Error::UnimplementedFeature {
                feature: "Column Encryption".to_string(),
                context: "Column encryption metadata parsing not yet supported".to_string(),
            });
        }

        // Handle the special case where no metadata is sent (0xFFFF)
        // This occurs for non-query statements like INSERT, UPDATE, DELETE
        if col_count == 0xFFFF {
            return Ok(Tokens::from(ColMetadataToken::default()));
        }

        // Pre-allocate vector for column metadata
        let mut column_metadata: Vec<ColumnMetadata> = Vec::with_capacity(col_count as usize);

        // Parse each column definition
        for _ in 0..col_count {
            // User-defined type identifier (4 bytes)
            // 0 for built-in types, > 0 for UDTs
            let user_type = reader.read_uint32().await?;

            // Column flags (2 bytes) - see bitmask definition above
            let flags = reader.read_uint16().await?;

            // Data type byte (1 byte) - TDS type identifier
            let raw_data_type = reader.read_byte().await?;
            let some_data_type = TdsDataType::try_from(raw_data_type);
            if some_data_type.is_err() {
                return Err(crate::error::Error::from(Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("Invalid data type: {raw_data_type}"),
                )));
            }
            let data_type = some_data_type?;

            // Type-specific information (variable length)
            // Structure depends on data_type (see sqldatatypes.rs)
            // Type-specific information (variable length)
            // Structure depends on data_type (see sqldatatypes.rs)
            let type_info = read_type_info(reader, data_type).await?;

            // Parse Table name (optional - only for TEXT/NTEXT/IMAGE types)
            // TDS Spec: "The fully qualified base table name for this column.
            // It contains the table name length and table name.
            // This exists only for text, ntext, and image columns."
            // Format: NumParts(1 byte) followed by PartName repeated NumParts times
            // Parts are sent in order: Server, Catalog(DB), Schema, Table
            let multi_part_name = match data_type {
                TdsDataType::Text | TdsDataType::NText | TdsDataType::Image => {
                    let mut part_count = reader.read_byte().await?;
                    if part_count == 0 {
                        None
                    } else {
                        let mut mpt = MultiPartName::default();
                        while part_count > 0 {
                            let part_name = reader.read_varchar_u16_length().await?;
                            // Parse multi-part name in reverse order
                            // 4 = server, 3 = catalog, 2 = schema, 1 = table
                            if part_count == 4 {
                                mpt.server_name = part_name;
                            } else if part_count == 3 {
                                mpt.catalog_name = part_name;
                            } else if part_count == 2 {
                                mpt.schema_name = part_name;
                            } else if part_count == 1 {
                                mpt.table_name = part_name.unwrap_or_default();
                            }
                            part_count -= 1;
                        }
                        Some(mpt)
                    }
                }
                _ => None,
            };

            // Read column name (B_VARCHAR with 1-byte length prefix)
            let col_name = reader.read_varchar_u8_length().await?;

            // Construct column metadata
            let col_metadata = ColumnMetadata {
                user_type,
                flags,
                data_type,
                type_info,
                column_name: col_name,
                multi_part_name,
            };

            // Check for Always Encrypted columns (not yet supported)
            if col_metadata.is_encrypted() {
                return Err(crate::error::Error::ProtocolError(
                    "Column encryption is not yet supported".to_string(),
                ));
            }

            column_metadata.push(col_metadata);
        }

        // Construct the complete metadata token
        let metadata = ColMetadataToken {
            column_count: col_count,
            columns: column_metadata,
        };
        Ok(Tokens::from(metadata))
    }
}

#[cfg(test)]
mod tests {
    use super::super::common::test_utils::MockReader;
    use super::*;
    use crate::datatypes::sqldatatypes::TdsDataType;
    use byteorder::{ByteOrder, LittleEndian};

    /// Helper to build column metadata bytes
    fn build_colmetadata_bytes(col_count: u16, columns: Vec<ColumnData>) -> Vec<u8> {
        let mut data = Vec::new();

        // Write column count
        let mut buf = [0u8; 2];
        LittleEndian::write_u16(&mut buf, col_count);
        data.extend_from_slice(&buf);

        // Write each column
        for col in columns {
            // UserType (4 bytes)
            let mut buf = [0u8; 4];
            LittleEndian::write_u32(&mut buf, col.user_type);
            data.extend_from_slice(&buf);

            // Flags (2 bytes)
            let mut buf = [0u8; 2];
            LittleEndian::write_u16(&mut buf, col.flags);
            data.extend_from_slice(&buf);

            // DataType (1 byte)
            data.push(col.data_type_byte);

            // TypeInfo (varies by type)
            data.extend_from_slice(&col.type_info_bytes);

            // Column name (B_VARCHAR with 1-byte length)
            let name_bytes = MockReader::encode_utf16(&col.name);
            data.push((name_bytes.len() / 2) as u8); // Length in characters
            data.extend_from_slice(&name_bytes);
        }

        data
    }

    #[derive(Clone)]
    struct ColumnData {
        user_type: u32,
        flags: u16,
        data_type_byte: u8,
        type_info_bytes: Vec<u8>,
        name: String,
    }

    #[tokio::test]
    async fn test_parse_no_metadata() {
        // 0xFFFF indicates no metadata
        let data = vec![0xFF, 0xFF];
        let mut reader = MockReader::new(data);
        let parser = ColMetadataTokenParser::default();
        let context = ParserContext::default();

        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::ColMetadata(token) => {
                assert_eq!(token.column_count, 0);
                assert_eq!(token.columns.len(), 0);
            }
            _ => panic!("Expected ColMetadata token"),
        }
    }

    #[tokio::test]
    async fn test_parse_single_int_column() {
        let columns = vec![ColumnData {
            user_type: 0,
            flags: 0x00, // Not nullable
            data_type_byte: TdsDataType::Int4 as u8,
            type_info_bytes: vec![], // INT has no type info
            name: "id".to_string(),
        }];

        let data = build_colmetadata_bytes(1, columns);
        let mut reader = MockReader::new(data);
        let parser = ColMetadataTokenParser::default();
        let context = ParserContext::default();

        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::ColMetadata(token) => {
                assert_eq!(token.column_count, 1);
                assert_eq!(token.columns.len(), 1);
                assert_eq!(token.columns[0].user_type, 0);
                assert_eq!(token.columns[0].flags, 0x00);
                assert_eq!(token.columns[0].data_type, TdsDataType::Int4);
                assert_eq!(token.columns[0].column_name, "id");
            }
            _ => panic!("Expected ColMetadata token"),
        }
    }

    #[tokio::test]
    async fn test_parse_nullable_column() {
        let columns = vec![ColumnData {
            user_type: 0,
            flags: 0x01, // Nullable
            data_type_byte: TdsDataType::IntN as u8,
            type_info_bytes: vec![0x04], // IntN type info: length byte
            name: "age".to_string(),
        }];

        let data = build_colmetadata_bytes(1, columns);
        let mut reader = MockReader::new(data);
        let parser = ColMetadataTokenParser::default();
        let context = ParserContext::default();

        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::ColMetadata(token) => {
                assert_eq!(token.columns.len(), 1);
                assert_eq!(token.columns[0].flags, 0x01);
                assert_eq!(token.columns[0].data_type, TdsDataType::IntN);
                assert!(token.columns[0].is_nullable());
            }
            _ => panic!("Expected ColMetadata token"),
        }
    }

    #[tokio::test]
    async fn test_parse_multiple_columns() {
        let columns = vec![
            ColumnData {
                user_type: 0,
                flags: 0x00,
                data_type_byte: TdsDataType::Int4 as u8,
                type_info_bytes: vec![],
                name: "id".to_string(),
            },
            ColumnData {
                user_type: 0,
                flags: 0x01,
                data_type_byte: TdsDataType::IntN as u8,
                type_info_bytes: vec![0x04],
                name: "age".to_string(),
            },
            ColumnData {
                user_type: 0,
                flags: 0x01,
                data_type_byte: TdsDataType::BigVarChar as u8,
                type_info_bytes: {
                    let mut bytes = vec![
                        0x32, 0x00, // MaxLength: 50
                    ];
                    // Collation (5 bytes): LCID + flags
                    bytes.extend_from_slice(&[0x09, 0x04, 0xD0, 0x00, 0x34]);
                    bytes
                },
                name: "name".to_string(),
            },
        ];

        let data = build_colmetadata_bytes(3, columns.clone());
        let mut reader = MockReader::new(data);
        let parser = ColMetadataTokenParser::default();
        let context = ParserContext::default();

        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::ColMetadata(token) => {
                assert_eq!(token.column_count, 3);
                assert_eq!(token.columns.len(), 3);

                // Check first column
                assert_eq!(token.columns[0].column_name, "id");
                assert_eq!(token.columns[0].data_type, TdsDataType::Int4);

                // Check second column
                assert_eq!(token.columns[1].column_name, "age");
                assert_eq!(token.columns[1].data_type, TdsDataType::IntN);

                // Check third column
                assert_eq!(token.columns[2].column_name, "name");
                assert_eq!(token.columns[2].data_type, TdsDataType::BigVarChar);
            }
            _ => panic!("Expected ColMetadata token"),
        }
    }

    #[tokio::test]
    async fn test_parse_bigint_column() {
        let columns = vec![ColumnData {
            user_type: 0,
            flags: 0x00,
            data_type_byte: TdsDataType::Int8 as u8,
            type_info_bytes: vec![],
            name: "bigid".to_string(),
        }];

        let data = build_colmetadata_bytes(1, columns);
        let mut reader = MockReader::new(data);
        let parser = ColMetadataTokenParser::default();
        let context = ParserContext::default();

        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::ColMetadata(token) => {
                assert_eq!(token.columns.len(), 1);
                assert_eq!(token.columns[0].data_type, TdsDataType::Int8);
            }
            _ => panic!("Expected ColMetadata token"),
        }
    }

    #[tokio::test]
    async fn test_parse_identity_column() {
        let columns = vec![ColumnData {
            user_type: 0,
            flags: 0x10, // Identity flag (per ColumnMetadata::is_identity implementation)
            data_type_byte: TdsDataType::Int4 as u8,
            type_info_bytes: vec![],
            name: "id".to_string(),
        }];

        let data = build_colmetadata_bytes(1, columns);
        let mut reader = MockReader::new(data);
        let parser = ColMetadataTokenParser::default();
        let context = ParserContext::default();

        let result = parser.parse(&mut reader, &context).await.unwrap();

        match result {
            Tokens::ColMetadata(token) => {
                assert_eq!(token.columns.len(), 1);
                assert_eq!(token.columns[0].flags, 0x10);
                assert!(token.columns[0].is_identity());
            }
            _ => panic!("Expected ColMetadata token"),
        }
    }

    #[tokio::test]
    async fn test_invalid_data_type() {
        let mut data = vec![0x01, 0x00]; // col_count = 1
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // user_type
        data.extend_from_slice(&[0x00, 0x00]); // flags
        data.push(0xFF); // Invalid data type byte

        let mut reader = MockReader::new(data);
        let parser = ColMetadataTokenParser::default();
        let context = ParserContext::default();

        let result = parser.parse(&mut reader, &context).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_column_encryption_not_supported() {
        let parser = ColMetadataTokenParser {
            is_column_encryption_supported: true,
        }; // Enable encryption support
        let data = vec![0x01, 0x00]; // col_count = 1
        let mut reader = MockReader::new(data);
        let context = ParserContext::default();

        let result = parser.parse(&mut reader, &context).await;
        assert!(result.is_err());

        if let Err(crate::error::Error::UnimplementedFeature { feature, .. }) = result {
            assert_eq!(feature, "Column Encryption");
        } else {
            panic!("Expected UnimplementedFeature error");
        }
    }
}
