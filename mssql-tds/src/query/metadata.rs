// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::{
    datatypes::sqldatatypes::{TdsDataType, TypeInfo, TypeInfoVariant},
    token::tokens::SqlCollation,
};

use std::fmt;

/// Schema, type, and flag information for a single column in a result set.
///
/// Returned as part of the `COLMETADATA` TDS token; one instance per column.
#[derive(Debug, Clone)]
pub struct ColumnMetadata {
    /// SQL Server user-type ordinal (0 for base types, non-zero for aliases/CLR types).
    pub user_type: u32,
    /// Bitmask of column property flags (nullable, identity, computed, etc.).
    pub flags: u16,
    /// Wire-level type descriptor including length, precision, and scale.
    pub type_info: TypeInfo,
    /// High-level SQL Server data type.
    pub data_type: TdsDataType,
    /// Column name as declared in the query or table definition.
    pub column_name: String,
    /// Optional four-part name (`server.catalog.schema.table`) when the server
    /// includes table-name metadata (e.g., browse-mode queries).
    pub multi_part_name: Option<MultiPartName>,
    /// Always Encrypted cryptographic metadata, present only for encrypted
    /// columns when column encryption has been negotiated for the connection.
    ///
    /// When set, `data_type`/`type_info` describe the on-the-wire (ciphertext)
    /// type, while the contained [`CryptoMetadata`] carries the underlying
    /// plaintext type and the key/algorithm needed to decrypt the column.
    #[allow(dead_code)] // Populated during COLMETADATA parsing; consumed by result-set decryption in a later phase.
    pub(crate) crypto_metadata: Option<CryptoMetadata>,
}

impl ColumnMetadata {
    /// Column allows `NULL` values.
    pub fn is_nullable(&self) -> bool {
        (self.flags & 0x01) != 0x00
    }
    /// Column uses a case-sensitive collation.
    pub fn is_case_sensitive(&self) -> bool {
        (self.flags & 0x02) != 0x00
    }
    /// Column is an identity (auto-increment) column.
    pub fn is_identity(&self) -> bool {
        (self.flags & 0x10) != 0x00
    }
    /// Column value is computed by the server.
    pub fn is_computed(&self) -> bool {
        (self.flags & 0x20) != 0x00
    }
    /// Column is a sparse column set (`xml COLUMN_SET FOR ALL_SPARSE_COLUMNS`),
    /// `fSparseColumnSet`, bit 10 of the COLMETADATA Flags word.
    pub fn is_sparse_column_set(&self) -> bool {
        (self.flags & 0x0400) != 0x00
    }
    /// Column is protected by Always Encrypted (`fEncrypted`, bit 11 of the
    /// COLMETADATA Flags word).
    pub fn is_encrypted(&self) -> bool {
        (self.flags & 0x0800) != 0x00
    }
    /// Column is hidden (`fHidden`, bit 13 of the COLMETADATA Flags word), for
    /// example a `FOR BROWSE` key column.
    pub fn is_hidden(&self) -> bool {
        (self.flags & 0x2000) != 0x00
    }
    /// Column is a key column used in cursor operations (`fKey`, bit 14 of the
    /// COLMETADATA Flags word). A driver uses this to build positioned-update
    /// predicates (`WHERE` clauses) for updatable server cursors.
    pub fn is_key_column(&self) -> bool {
        (self.flags & 0x4000) != 0x00
    }
    /// Column uses Partially Length-prefixed (PLP) encoding (e.g., `varchar(max)`).
    pub fn is_plp(&self) -> bool {
        matches!(
            self.type_info.type_info_variant,
            TypeInfoVariant::PartialLen(_, _, _, _, _)
        )
    }
    /// Returns the scale for decimal/numeric/time types.
    ///
    /// Returns `Some(scale)` for types that include scale information (e.g., `decimal(18,4)`, `time(7)`),
    /// or `None` for types where scale is not applicable.
    pub fn get_scale(&self) -> Option<u8> {
        match self.type_info.type_info_variant {
            TypeInfoVariant::VarLenScale(_, scale) => Some(scale),
            TypeInfoVariant::VarLenPrecisionScale(_, _, _, scale) => Some(scale),
            _ => None,
        }
    }

    /// Returns the precision for decimal/numeric types.
    ///
    /// Returns `Some(precision)` for types that include precision information
    /// (e.g., `decimal(18,4)`, `numeric(38,0)`), or `None` for types where
    /// precision is not applicable.
    pub fn get_precision(&self) -> Option<u8> {
        match self.type_info.type_info_variant {
            TypeInfoVariant::VarLenPrecisionScale(_, _, precision, _) => Some(precision),
            _ => None,
        }
    }

    /// Returns the SQL collation for string-typed columns, or `None` for non-string types.
    pub fn get_collation(&self) -> Option<SqlCollation> {
        // Collation is only applicable to string types which are either VarLen strings
        // Or PLP types with a collation.
        match self.type_info.type_info_variant {
            TypeInfoVariant::VarLenString(_, _, collation) => collation,
            TypeInfoVariant::PartialLen(_, _, collation, _, _) => collation,
            _ => None,
        }
    }
}

impl fmt::Display for ColumnMetadata {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Column Name: {}\nData Type: {:?} (UserType: {})\nFlags: [Nullable: {}, CaseSensitive: {}, Identity: {}, Computed: {}, \
        SparseColumnSet: {}, Encrypted: {}, MultiPartName: {:?}]\n",
            self.column_name,
            self.data_type,
            self.user_type,
            self.is_nullable(),
            self.is_case_sensitive(),
            self.is_identity(),
            self.is_computed(),
            self.is_sparse_column_set(),
            self.is_encrypted(),
            self.multi_part_name
        )
    }
}

/// Four-part table name (`server.catalog.schema.table`) optionally returned
/// by the server alongside column metadata in browse-mode queries.
#[derive(Debug, Default, Clone)]
pub struct MultiPartName {
    pub(crate) server_name: Option<String>,
    pub(crate) catalog_name: Option<String>,
    pub(crate) schema_name: Option<String>,
    pub(crate) table_name: String,
}

impl MultiPartName {
    /// Server name component, if provided by the server.
    pub fn server_name(&self) -> Option<&str> {
        self.server_name.as_deref()
    }

    /// Catalog (database) name component, if provided by the server.
    pub fn catalog_name(&self) -> Option<&str> {
        self.catalog_name.as_deref()
    }

    /// Schema name component, if provided by the server.
    pub fn schema_name(&self) -> Option<&str> {
        self.schema_name.as_deref()
    }

    /// Table name component. Always present when a `MultiPartName` is returned
    /// (may be empty if the server sent an empty string).
    pub fn table_name(&self) -> &str {
        &self.table_name
    }
}

/// A single encrypted value of a column encryption key (CEK), as carried in the
/// COLMETADATA CEK table.
///
/// A CEK may have more than one encrypted value (one per column master key that
/// wraps it) to support key rotation; any one of them can be used to recover the
/// plaintext CEK.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields consumed by CEK decryption in a later phase.
pub(crate) struct EncryptedCekValue {
    /// The encrypted CEK bytes (ciphertext produced by the column master key).
    pub encrypted_key: Vec<u8>,
    /// Name of the key store provider (e.g. `MSSQL_CERTIFICATE_STORE`, `AZURE_KEY_VAULT`).
    pub key_store_name: String,
    /// Provider-specific path identifying the column master key.
    pub key_path: String,
    /// Name of the asymmetric algorithm used to encrypt the CEK (e.g. `RSA_OAEP`).
    pub algorithm_name: String,
}

/// One entry in the COLMETADATA CEK table, describing a column encryption key
/// and the encrypted values from which its plaintext can be recovered.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields consumed by CEK decryption in a later phase.
pub(crate) struct CekTableEntry {
    /// Database identifier the CEK belongs to.
    pub database_id: i32,
    /// CEK identifier.
    pub cek_id: i32,
    /// CEK version.
    pub cek_version: i32,
    /// CEK metadata version (8 bytes).
    pub cek_md_version: [u8; 8],
    /// Encrypted CEK values, one per column master key that wraps this CEK.
    pub encrypted_cek_values: Vec<EncryptedCekValue>,
}

/// Per-column cryptographic metadata for an Always Encrypted column, parsed from
/// the COLMETADATA token when column encryption has been negotiated.
#[derive(Debug, Clone)]
#[allow(dead_code)] // Fields consumed by result-set decryption in a later phase.
pub(crate) struct CryptoMetadata {
    /// Ordinal into the result set's CEK table identifying the wrapping key.
    pub cek_table_ordinal: u16,
    /// Underlying SQL Server data type of the column before encryption.
    pub base_data_type: TdsDataType,
    /// Wire descriptor for the underlying (plaintext) type.
    pub base_type_info: TypeInfo,
    /// Cipher algorithm identifier (`0x01` = AEAD_AES_256_CBC_HMAC_SHA256, `0x00` = custom).
    pub cipher_algorithm_id: u8,
    /// Custom cipher algorithm name; present only when `cipher_algorithm_id` is `0`.
    pub cipher_algorithm_name: Option<String>,
    /// Encryption type byte (`1` = deterministic, `2` = randomized).
    pub encryption_type: u8,
    /// Normalization rule version byte.
    pub normalization_rule_version: u8,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datatypes::sqldatatypes::{
        FixedLengthTypes, PartialLengthType, TdsDataType, TypeInfo, TypeInfoVariant,
        VariableLengthTypes,
    };
    use crate::token::tokens::SqlCollation;

    fn create_test_column_metadata(
        flags: u16,
        type_info_variant: TypeInfoVariant,
    ) -> ColumnMetadata {
        ColumnMetadata {
            user_type: 0,
            flags,
            type_info: TypeInfo {
                tds_type: TdsDataType::IntN,
                length: 4,
                type_info_variant,
            },
            data_type: TdsDataType::IntN,
            column_name: "test_column".to_string(),
            multi_part_name: None,
            crypto_metadata: None,
        }
    }

    #[test]
    fn test_is_nullable() {
        let metadata =
            create_test_column_metadata(0x01, TypeInfoVariant::FixedLen(FixedLengthTypes::Int4));
        assert!(metadata.is_nullable());

        let metadata =
            create_test_column_metadata(0x00, TypeInfoVariant::FixedLen(FixedLengthTypes::Int4));
        assert!(!metadata.is_nullable());
    }

    #[test]
    fn test_is_case_sensitive() {
        let metadata =
            create_test_column_metadata(0x02, TypeInfoVariant::FixedLen(FixedLengthTypes::Int4));
        assert!(metadata.is_case_sensitive());

        let metadata =
            create_test_column_metadata(0x00, TypeInfoVariant::FixedLen(FixedLengthTypes::Int4));
        assert!(!metadata.is_case_sensitive());
    }

    #[test]
    fn test_is_identity() {
        let metadata =
            create_test_column_metadata(0x10, TypeInfoVariant::FixedLen(FixedLengthTypes::Int4));
        assert!(metadata.is_identity());

        let metadata =
            create_test_column_metadata(0x00, TypeInfoVariant::FixedLen(FixedLengthTypes::Int4));
        assert!(!metadata.is_identity());
    }

    #[test]
    fn test_is_computed() {
        let metadata =
            create_test_column_metadata(0x20, TypeInfoVariant::FixedLen(FixedLengthTypes::Int4));
        assert!(metadata.is_computed());

        let metadata =
            create_test_column_metadata(0x00, TypeInfoVariant::FixedLen(FixedLengthTypes::Int4));
        assert!(!metadata.is_computed());
    }

    #[test]
    fn test_is_sparse_column_set() {
        let metadata =
            create_test_column_metadata(0x0400, TypeInfoVariant::FixedLen(FixedLengthTypes::Int4));
        assert!(metadata.is_sparse_column_set());

        // 0x1000 is fUnused1, not fSparseColumnSet.
        let metadata =
            create_test_column_metadata(0x1000, TypeInfoVariant::FixedLen(FixedLengthTypes::Int4));
        assert!(!metadata.is_sparse_column_set());

        let metadata =
            create_test_column_metadata(0x00, TypeInfoVariant::FixedLen(FixedLengthTypes::Int4));
        assert!(!metadata.is_sparse_column_set());
    }

    #[test]
    fn test_is_encrypted() {
        let metadata =
            create_test_column_metadata(0x0800, TypeInfoVariant::FixedLen(FixedLengthTypes::Int4));
        assert!(metadata.is_encrypted());

        // 0x2000 is fHidden (FOR BROWSE), not fEncrypted.
        let metadata =
            create_test_column_metadata(0x2000, TypeInfoVariant::FixedLen(FixedLengthTypes::Int4));
        assert!(!metadata.is_encrypted());

        let metadata =
            create_test_column_metadata(0x00, TypeInfoVariant::FixedLen(FixedLengthTypes::Int4));
        assert!(!metadata.is_encrypted());
    }

    #[test]
    fn test_is_hidden() {
        let metadata =
            create_test_column_metadata(0x2000, TypeInfoVariant::FixedLen(FixedLengthTypes::Int4));
        assert!(metadata.is_hidden());

        let metadata =
            create_test_column_metadata(0x00, TypeInfoVariant::FixedLen(FixedLengthTypes::Int4));
        assert!(!metadata.is_hidden());
    }

    #[test]
    fn test_is_key_column() {
        let metadata =
            create_test_column_metadata(0x4000, TypeInfoVariant::FixedLen(FixedLengthTypes::Int4));
        assert!(metadata.is_key_column());

        let metadata =
            create_test_column_metadata(0x00, TypeInfoVariant::FixedLen(FixedLengthTypes::Int4));
        assert!(!metadata.is_key_column());
    }

    #[test]
    fn test_is_plp() {
        let metadata = create_test_column_metadata(
            0x00,
            TypeInfoVariant::PartialLen(PartialLengthType::BigVarChar, None, None, None, None),
        );
        assert!(metadata.is_plp());

        let metadata =
            create_test_column_metadata(0x00, TypeInfoVariant::FixedLen(FixedLengthTypes::Int4));
        assert!(!metadata.is_plp());
    }

    #[test]
    fn test_get_scale_varlen_scale() {
        let metadata = create_test_column_metadata(
            0x00,
            TypeInfoVariant::VarLenScale(VariableLengthTypes::TimeN, 7),
        );
        assert_eq!(metadata.get_scale(), Some(7));
    }

    #[test]
    fn test_get_scale_varlen_precision_scale() {
        let metadata = create_test_column_metadata(
            0x00,
            TypeInfoVariant::VarLenPrecisionScale(VariableLengthTypes::DecimalN, 18, 38, 4),
        );
        assert_eq!(metadata.get_scale(), Some(4));
    }

    #[test]
    fn test_get_scale_none() {
        let metadata =
            create_test_column_metadata(0x00, TypeInfoVariant::FixedLen(FixedLengthTypes::Int4));
        assert_eq!(metadata.get_scale(), None);
    }

    #[test]
    fn test_get_precision_varlen_precision_scale() {
        let metadata = create_test_column_metadata(
            0x00,
            TypeInfoVariant::VarLenPrecisionScale(VariableLengthTypes::DecimalN, 18, 38, 4),
        );
        assert_eq!(metadata.get_precision(), Some(38));
    }

    #[test]
    fn test_get_precision_varlen_scale_only_is_none() {
        let metadata = create_test_column_metadata(
            0x00,
            TypeInfoVariant::VarLenScale(VariableLengthTypes::TimeN, 7),
        );
        assert_eq!(metadata.get_precision(), None);
    }

    #[test]
    fn test_get_precision_none() {
        let metadata =
            create_test_column_metadata(0x00, TypeInfoVariant::FixedLen(FixedLengthTypes::Int4));
        assert_eq!(metadata.get_precision(), None);
    }

    #[test]
    fn test_get_collation_varlen_string() {
        let collation = SqlCollation {
            info: 0,
            lcid_language_id: 1033,
            col_flags: 0,
            sort_id: 0,
        };
        let metadata = create_test_column_metadata(
            0x00,
            TypeInfoVariant::VarLenString(VariableLengthTypes::BigVarChar, 100, Some(collation)),
        );
        assert!(metadata.get_collation().is_some());
    }

    #[test]
    fn test_get_collation_partial_len() {
        let collation = SqlCollation {
            info: 0,
            lcid_language_id: 1033,
            col_flags: 0,
            sort_id: 0,
        };
        let metadata = create_test_column_metadata(
            0x00,
            TypeInfoVariant::PartialLen(
                PartialLengthType::BigVarChar,
                None,
                Some(collation),
                None,
                None,
            ),
        );
        assert!(metadata.get_collation().is_some());
    }

    #[test]
    fn test_get_collation_none() {
        let metadata =
            create_test_column_metadata(0x00, TypeInfoVariant::FixedLen(FixedLengthTypes::Int4));
        assert!(metadata.get_collation().is_none());
    }

    #[test]
    fn test_display_format() {
        let metadata =
            create_test_column_metadata(0x01, TypeInfoVariant::FixedLen(FixedLengthTypes::Int4));
        let display = format!("{metadata}");
        assert!(display.contains("test_column"));
        assert!(display.contains("Nullable: true"));
    }

    #[test]
    fn test_multi_part_name_default() {
        let multi_part = MultiPartName::default();
        assert_eq!(multi_part.table_name, "");
        assert!(multi_part.server_name.is_none());
        assert!(multi_part.catalog_name.is_none());
        assert!(multi_part.schema_name.is_none());
    }

    #[test]
    fn test_multi_part_name_accessors() {
        let multi_part = MultiPartName {
            server_name: Some("server".to_string()),
            catalog_name: Some("catalog".to_string()),
            schema_name: Some("dbo".to_string()),
            table_name: "users".to_string(),
        };
        assert_eq!(multi_part.server_name(), Some("server"));
        assert_eq!(multi_part.catalog_name(), Some("catalog"));
        assert_eq!(multi_part.schema_name(), Some("dbo"));
        assert_eq!(multi_part.table_name(), "users");
    }

    #[test]
    fn test_multi_part_name_accessors_default() {
        let multi_part = MultiPartName::default();
        assert_eq!(multi_part.server_name(), None);
        assert_eq!(multi_part.catalog_name(), None);
        assert_eq!(multi_part.schema_name(), None);
        assert_eq!(multi_part.table_name(), "");
    }

    #[test]
    fn test_multi_part_name_clone() {
        let multi_part = MultiPartName {
            server_name: Some("server".to_string()),
            catalog_name: Some("catalog".to_string()),
            schema_name: Some("dbo".to_string()),
            table_name: "users".to_string(),
        };
        let cloned = multi_part.clone();
        assert_eq!(cloned.server_name, Some("server".to_string()));
        assert_eq!(cloned.table_name, "users");
    }

    #[test]
    fn test_column_metadata_clone() {
        let metadata =
            create_test_column_metadata(0x01, TypeInfoVariant::FixedLen(FixedLengthTypes::Int4));
        let cloned = metadata.clone();
        assert_eq!(cloned.column_name, "test_column");
        assert_eq!(cloned.flags, 0x01);
    }
}
