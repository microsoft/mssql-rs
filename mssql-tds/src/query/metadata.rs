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

    /// Returns the precision (max decimal digits) for numeric types.
    ///
    /// - `decimal`/`numeric` → declared precision (1–38).
    /// - `money` → 19, `smallmoney` → 10 (T-SQL fixed precisions).
    /// - `MoneyN` → 19 if 8-byte payload, 10 if 4-byte payload.
    /// - All other types → `None`.
    pub fn get_precision(&self) -> Option<u8> {
        use crate::datatypes::sqldatatypes::{FixedLengthTypes, VariableLengthTypes};

        match self.type_info.type_info_variant {
            TypeInfoVariant::VarLenPrecisionScale(_, _, precision, _) => Some(precision),
            TypeInfoVariant::FixedLen(FixedLengthTypes::Money) => Some(19),
            TypeInfoVariant::FixedLen(FixedLengthTypes::Money4) => Some(10),
            TypeInfoVariant::VarLen(VariableLengthTypes::MoneyN, length) => match length {
                8 => Some(19),
                4 => Some(10),
                _ => None,
            },
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

#[derive(Debug)]
#[allow(dead_code)] // For column encryption metadata which is not implemented yet.
pub(crate) struct ColumnEncryptionMetadata {
    pub key_count: u8,
    pub key_details: Vec<ColumnEncryptionKeyDetails>,
    pub db_id: u32,
    pub key_id: u32,
}

#[derive(Debug)]
#[allow(dead_code)] // For column encryption metadata which is not implemented yet.
pub(crate) struct ColumnEncryptionKeyDetails {
    pub encrypted_cek: Vec<u8>,
    pub algo: String,
    pub key_path: String,
    pub key_store_name: String,
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
    fn test_get_precision_decimal_numeric() {
        // VarLenPrecisionScale: declared precision returned verbatim, regardless of scale.
        let dec = create_test_column_metadata(
            0x00,
            TypeInfoVariant::VarLenPrecisionScale(VariableLengthTypes::DecimalN, 17, 38, 4),
        );
        assert_eq!(dec.get_precision(), Some(38));

        let num = create_test_column_metadata(
            0x00,
            TypeInfoVariant::VarLenPrecisionScale(VariableLengthTypes::NumericN, 9, 18, 0),
        );
        assert_eq!(num.get_precision(), Some(18));
    }

    #[test]
    fn test_get_precision_money_types() {
        // money/smallmoney have T-SQL fixed precisions; MoneyN dispatches on wire length.
        let cases = [
            (TypeInfoVariant::FixedLen(FixedLengthTypes::Money), Some(19)),
            (
                TypeInfoVariant::FixedLen(FixedLengthTypes::Money4),
                Some(10),
            ),
            (
                TypeInfoVariant::VarLen(VariableLengthTypes::MoneyN, 8),
                Some(19),
            ),
            (
                TypeInfoVariant::VarLen(VariableLengthTypes::MoneyN, 4),
                Some(10),
            ),
            // MoneyN only ever carries 4 or 8 on the wire; anything else is malformed.
            (
                TypeInfoVariant::VarLen(VariableLengthTypes::MoneyN, 6),
                None,
            ),
        ];
        for (variant, expected) in cases {
            let meta = create_test_column_metadata(0x00, variant);
            assert_eq!(meta.get_precision(), expected);
        }
    }

    #[test]
    fn test_get_precision_none_for_non_numeric() {
        // Variants that don't carry numeric precision all return None.
        let cases = [
            TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
            TypeInfoVariant::VarLen(VariableLengthTypes::IntN, 4),
            TypeInfoVariant::VarLenScale(VariableLengthTypes::TimeN, 7),
            TypeInfoVariant::PartialLen(PartialLengthType::BigVarChar, None, None, None, None),
        ];
        for variant in cases {
            let meta = create_test_column_metadata(0x00, variant);
            assert_eq!(meta.get_precision(), None);
        }
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
