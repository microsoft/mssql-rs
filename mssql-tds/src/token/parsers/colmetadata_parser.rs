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
//! When Always Encrypted is negotiated, a CEK (column encryption key) table is
//! sent immediately after `ColCount`, and each encrypted column carries a
//! `CryptoMetadata` blob after its (ciphertext) `TypeInfo`.
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
    query::metadata::{
        CekTableEntry, ColumnMetadata, CryptoMetadata, EncryptedCekValue, MultiPartName,
    },
    token::tokens::ColMetadataToken,
};

/// Column-flag bit indicating the column is protected by Always Encrypted.
const FLAG_ENCRYPTED: u16 = 0x0800;

/// Cipher algorithm id signalling a custom (named) algorithm whose name follows
/// inline in the crypto metadata.
const CUSTOM_CIPHER_ALGORITHM_ID: u8 = 0x00;

/// Parser for COLMETADATA token (0x81) - defines result set column schema
#[derive(Default)]
pub(crate) struct ColMetadataTokenParser;

#[async_trait]
impl<T> TokenParser<T> for ColMetadataTokenParser
where
    T: TdsPacketReader + Send + Sync,
{
    async fn parse(&self, reader: &mut T, context: &ParserContext) -> TdsResult<Tokens> {
        // Whether Always Encrypted (TCE) was negotiated for this connection.
        // When set, the COLMETADATA token carries a CEK table and per-column
        // crypto metadata.
        let is_column_encryption_supported = context.is_column_encryption_supported();

        // Read column count (2 bytes)
        // Special value 0xFFFF indicates no metadata (used for INSERT/UPDATE/DELETE)
        let col_count = reader.read_uint16().await?;

        // Handle the special case where no metadata is sent (0xFFFF)
        // This occurs for non-query statements like INSERT, UPDATE, DELETE
        if col_count == 0xFFFF {
            return Ok(Tokens::from(ColMetadataToken::default()));
        }

        // When column encryption is negotiated, the CEK table is sent right
        // after the column count (and before any column definitions). It is
        // empty when none of the columns are encrypted.
        let cek_table = if is_column_encryption_supported {
            parse_cek_table(reader).await?
        } else {
            Vec::new()
        };
        let has_cek_table = !cek_table.is_empty();

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

            // Always Encrypted: encrypted columns carry a CryptoMetadata blob
            // after the (ciphertext) TYPE_INFO / table name and before the
            // column name. The outer `data_type`/`type_info` describe the
            // on-the-wire encrypted type; the crypto metadata carries the
            // underlying plaintext type plus the key and algorithm details.
            let crypto_metadata = if is_column_encryption_supported && (flags & FLAG_ENCRYPTED) != 0
            {
                Some(parse_crypto_metadata(reader, has_cek_table).await?)
            } else {
                None
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
                crypto_metadata,
            };

            column_metadata.push(col_metadata);
        }

        // Construct the complete metadata token
        let metadata = ColMetadataToken {
            column_count: col_count,
            columns: column_metadata,
            cek_table,
        };
        Ok(Tokens::from(metadata))
    }
}

/// Parses the CEK (column encryption key) table that precedes the column
/// definitions when Always Encrypted is negotiated.
///
/// Layout: `Count(u16)` followed by `Count` CEK table entries. A count of zero
/// means no columns in the result set are encrypted.
async fn parse_cek_table<T>(reader: &mut T) -> TdsResult<Vec<CekTableEntry>>
where
    T: TdsPacketReader + Send + Sync,
{
    let table_size = reader.read_uint16().await?;
    let mut entries: Vec<CekTableEntry> = Vec::with_capacity(table_size as usize);
    for _ in 0..table_size {
        entries.push(parse_cek_table_entry(reader).await?);
    }
    Ok(entries)
}

/// Parses a single CEK table entry: key identity metadata followed by one or
/// more encrypted CEK values (one per column master key that wraps the CEK).
async fn parse_cek_table_entry<T>(reader: &mut T) -> TdsResult<CekTableEntry>
where
    T: TdsPacketReader + Send + Sync,
{
    let database_id = reader.read_int32().await?;
    let cek_id = reader.read_int32().await?;
    let cek_version = reader.read_int32().await?;
    let mut cek_md_version = [0u8; 8];
    reader.read_bytes(&mut cek_md_version).await?;

    let value_count = reader.read_byte().await?;
    let mut encrypted_cek_values: Vec<EncryptedCekValue> =
        Vec::with_capacity(value_count as usize);
    for _ in 0..value_count {
        // Encrypted CEK blob (length-prefixed by a u16 byte count).
        let encrypted_len = reader.read_uint16().await? as usize;
        let mut encrypted_key = vec![0u8; encrypted_len];
        reader.read_bytes(&mut encrypted_key).await?;

        // Key store provider name (1-byte char count).
        let key_store_len = reader.read_byte().await? as usize;
        let key_store_name = reader.read_unicode(key_store_len).await?;

        // Column master key path (2-byte char count).
        let key_path_len = reader.read_uint16().await? as usize;
        let key_path = reader.read_unicode(key_path_len).await?;

        // Asymmetric key-wrapping algorithm name (1-byte char count).
        let algorithm_len = reader.read_byte().await? as usize;
        let algorithm_name = reader.read_unicode(algorithm_len).await?;

        encrypted_cek_values.push(EncryptedCekValue {
            encrypted_key,
            key_store_name,
            key_path,
            algorithm_name,
        });
    }

    Ok(CekTableEntry {
        database_id,
        cek_id,
        cek_version,
        cek_md_version,
        encrypted_cek_values,
    })
}

/// Parses the per-column CryptoMetadata that follows the ciphertext TYPE_INFO of
/// an encrypted column.
///
/// Layout: `CekTableOrdinal(u16, only when a CEK table is present)`, base
/// `TYPE_INFO`, `CipherAlgorithmId(u8)` (+ optional custom algorithm name),
/// `EncryptionType(u8)`, `NormalizationRuleVersion(u8)`.
async fn parse_crypto_metadata<T>(
    reader: &mut T,
    has_cek_table: bool,
) -> TdsResult<CryptoMetadata>
where
    T: TdsPacketReader + Send + Sync,
{
    // The ordinal into the CEK table is only present for result sets (which
    // carry a CEK table); it is absent for RPC return values.
    let cek_table_ordinal = if has_cek_table {
        reader.read_uint16().await?
    } else {
        0
    };

    // Base (plaintext) TYPE_INFO: a data-type byte followed by type-specific info.
    let raw_base_type = reader.read_byte().await?;
    let base_data_type = TdsDataType::try_from(raw_base_type).map_err(|_| {
        crate::error::Error::from(Error::new(
            std::io::ErrorKind::InvalidData,
            format!("Invalid base data type in crypto metadata: {raw_base_type}"),
        ))
    })?;
    let base_type_info = read_type_info(reader, base_data_type).await?;

    let cipher_algorithm_id = reader.read_byte().await?;
    let cipher_algorithm_name = if cipher_algorithm_id == CUSTOM_CIPHER_ALGORITHM_ID {
        let name_len = reader.read_byte().await? as usize;
        Some(reader.read_unicode(name_len).await?)
    } else {
        None
    };

    let encryption_type = reader.read_byte().await?;
    let normalization_rule_version = reader.read_byte().await?;

    Ok(CryptoMetadata {
        cek_table_ordinal,
        base_data_type,
        base_type_info,
        cipher_algorithm_id,
        cipher_algorithm_name,
        encryption_type,
        normalization_rule_version,
    })
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
        let parser = ColMetadataTokenParser;
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
        let parser = ColMetadataTokenParser;
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
        let parser = ColMetadataTokenParser;
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
        let parser = ColMetadataTokenParser;
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
        let parser = ColMetadataTokenParser;
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
        let parser = ColMetadataTokenParser;
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
        let parser = ColMetadataTokenParser;
        let context = ParserContext::default();

        let result = parser.parse(&mut reader, &context).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_column_encryption_empty_cek_table_no_encrypted_columns() {
        // Column encryption negotiated, but no encrypted columns: an empty CEK
        // table (size 0) precedes a single plain INT column.
        let mut data = Vec::new();
        data.extend_from_slice(&[0x01, 0x00]); // col_count = 1
        data.extend_from_slice(&[0x00, 0x00]); // CEK table size = 0
        // Column 0: plain INT
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // user_type
        data.extend_from_slice(&[0x00, 0x00]); // flags (not encrypted)
        data.push(TdsDataType::Int4 as u8); // data type (no type info)
        let name = MockReader::encode_utf16("id");
        data.push((name.len() / 2) as u8);
        data.extend_from_slice(&name);

        let mut reader = MockReader::new(data);
        let parser = ColMetadataTokenParser;
        let context = ParserContext::ColumnEncryption(true);

        let result = parser.parse(&mut reader, &context).await.unwrap();
        match result {
            Tokens::ColMetadata(token) => {
                assert_eq!(token.column_count, 1);
                assert!(token.cek_table.is_empty());
                assert_eq!(token.columns.len(), 1);
                assert!(token.columns[0].crypto_metadata.is_none());
                assert!(!token.columns[0].is_encrypted());
            }
            _ => panic!("Expected ColMetadata token"),
        }
    }

    #[tokio::test]
    async fn test_column_encryption_encrypted_column_with_cek_table() {
        // Column encryption negotiated with one CEK table entry and one
        // encrypted column carrying crypto metadata.
        let mut data = Vec::new();
        data.extend_from_slice(&[0x01, 0x00]); // col_count = 1

        // CEK table with a single entry.
        data.extend_from_slice(&[0x01, 0x00]); // CEK table size = 1
        data.extend_from_slice(&5i32.to_le_bytes()); // database_id
        data.extend_from_slice(&7i32.to_le_bytes()); // cek_id
        data.extend_from_slice(&1i32.to_le_bytes()); // cek_version
        data.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]); // cek_md_version
        data.push(0x01); // value_count = 1
        // Encrypted CEK value 0.
        data.extend_from_slice(&[0x04, 0x00]); // encrypted_len = 4
        data.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]); // encrypted_key
        data.push(0x03); // key_store_name len (chars)
        data.extend_from_slice(&MockReader::encode_utf16("AKV"));
        data.extend_from_slice(&[0x03, 0x00]); // key_path len (chars)
        data.extend_from_slice(&MockReader::encode_utf16("url"));
        data.push(0x03); // algorithm_name len (chars)
        data.extend_from_slice(&MockReader::encode_utf16("RSA"));

        // Column 0: encrypted. Outer (ciphertext) type is INT here for test
        // simplicity; the real wire type is varbinary, which the parser treats
        // generically.
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // user_type
        data.extend_from_slice(&FLAG_ENCRYPTED.to_le_bytes()); // flags = encrypted
        data.push(TdsDataType::Int4 as u8); // outer data type (no type info)
        // CryptoMetadata.
        data.extend_from_slice(&[0x00, 0x00]); // cek_table_ordinal = 0
        data.push(TdsDataType::Int4 as u8); // base data type (no type info)
        data.push(0x01); // cipher_algorithm_id (AEAD_AES_256_CBC_HMAC_SHA256)
        data.push(0x01); // encryption_type (deterministic)
        data.push(0x01); // normalization_rule_version
        // Column name.
        let name = MockReader::encode_utf16("secret");
        data.push((name.len() / 2) as u8);
        data.extend_from_slice(&name);

        let mut reader = MockReader::new(data);
        let parser = ColMetadataTokenParser;
        let context = ParserContext::ColumnEncryption(true);

        let result = parser.parse(&mut reader, &context).await.unwrap();
        match result {
            Tokens::ColMetadata(token) => {
                assert_eq!(token.column_count, 1);

                // CEK table.
                assert_eq!(token.cek_table.len(), 1);
                let entry = &token.cek_table[0];
                assert_eq!(entry.database_id, 5);
                assert_eq!(entry.cek_id, 7);
                assert_eq!(entry.cek_version, 1);
                assert_eq!(entry.cek_md_version, [1, 2, 3, 4, 5, 6, 7, 8]);
                assert_eq!(entry.encrypted_cek_values.len(), 1);
                let value = &entry.encrypted_cek_values[0];
                assert_eq!(value.encrypted_key, vec![0xDE, 0xAD, 0xBE, 0xEF]);
                assert_eq!(value.key_store_name, "AKV");
                assert_eq!(value.key_path, "url");
                assert_eq!(value.algorithm_name, "RSA");

                // Encrypted column + crypto metadata.
                assert_eq!(token.columns.len(), 1);
                let col = &token.columns[0];
                assert_eq!(col.column_name, "secret");
                assert!(col.is_encrypted());
                let crypto = col.crypto_metadata.as_ref().expect("crypto metadata");
                assert_eq!(crypto.cek_table_ordinal, 0);
                assert_eq!(crypto.base_data_type, TdsDataType::Int4);
                assert_eq!(crypto.cipher_algorithm_id, 0x01);
                assert!(crypto.cipher_algorithm_name.is_none());
                assert_eq!(crypto.encryption_type, 1);
                assert_eq!(crypto.normalization_rule_version, 1);
            }
            _ => panic!("Expected ColMetadata token"),
        }
    }

    #[tokio::test]
    async fn test_column_encryption_disabled_skips_cek_table() {
        // When column encryption is not negotiated, no CEK table is parsed and
        // the bytes are interpreted purely as column definitions.
        let columns = vec![ColumnData {
            user_type: 0,
            flags: 0x00,
            data_type_byte: TdsDataType::Int4 as u8,
            type_info_bytes: vec![],
            name: "id".to_string(),
        }];
        let data = build_colmetadata_bytes(1, columns);
        let mut reader = MockReader::new(data);
        let parser = ColMetadataTokenParser;
        let context = ParserContext::None(());

        let result = parser.parse(&mut reader, &context).await.unwrap();
        match result {
            Tokens::ColMetadata(token) => {
                assert_eq!(token.column_count, 1);
                assert!(token.cek_table.is_empty());
                assert!(token.columns[0].crypto_metadata.is_none());
            }
            _ => panic!("Expected ColMetadata token"),
        }
    }
}
