// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

//! Column type enumeration mirroring tiberius' ColumnType.

use mssql_tds::datatypes::sqldatatypes::TdsDataType;

/// SQL Server column data types, providing a tiberius-compatible enum
/// for pattern matching in application code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnType {
    Null,
    Bit,
    Int1,
    Int2,
    Int4,
    Int8,
    Float4,
    Float8,
    Datetime,
    Datetime2,
    Datetime4,
    DatetimeOffset,
    Date,
    Time,
    Decimaln,
    Numericn,
    Money,
    Money4,
    Guid,
    NVarchar,
    Varchar,
    NChar,
    Char,
    NText,
    Text,
    Binary,
    VarBinary,
    Image,
    Xml,
    Json,
    Vector,
    BigVarBin,
    Ssvariant,
    /// SQL Server `geography`, returned as raw serialized bytes.
    Geography,
    /// SQL Server `geometry`, returned as raw serialized bytes.
    Geometry,
    /// A CLR user-defined type without a recognized spatial identity.
    Udt,
}

impl From<TdsDataType> for ColumnType {
    fn from(dt: TdsDataType) -> Self {
        match dt {
            TdsDataType::Void => ColumnType::Null,
            TdsDataType::Bit | TdsDataType::BitN => ColumnType::Bit,
            TdsDataType::Int1 => ColumnType::Int1,
            TdsDataType::Int2 => ColumnType::Int2,
            TdsDataType::Int4 => ColumnType::Int4,
            TdsDataType::IntN => ColumnType::Int4, // Default; use from_tds_with_length for accuracy
            TdsDataType::Int8 => ColumnType::Int8,
            TdsDataType::Flt4 => ColumnType::Float4,
            TdsDataType::Flt8 => ColumnType::Float8,
            TdsDataType::FltN => ColumnType::Float8,
            TdsDataType::DateTime | TdsDataType::DateTimeN => ColumnType::Datetime,
            TdsDataType::DateTime2N => ColumnType::Datetime2,
            TdsDataType::DateTim4 => ColumnType::Datetime4,
            TdsDataType::DateTimeOffsetN => ColumnType::DatetimeOffset,
            TdsDataType::DateN => ColumnType::Date,
            TdsDataType::TimeN => ColumnType::Time,
            TdsDataType::Decimal | TdsDataType::DecimalN => ColumnType::Decimaln,
            TdsDataType::Numeric | TdsDataType::NumericN => ColumnType::Numericn,
            TdsDataType::Money | TdsDataType::MoneyN => ColumnType::Money,
            TdsDataType::Money4 => ColumnType::Money4,
            TdsDataType::Guid => ColumnType::Guid,
            TdsDataType::NVarChar => ColumnType::NVarchar,
            TdsDataType::VarChar | TdsDataType::BigVarChar => ColumnType::Varchar,
            TdsDataType::NChar => ColumnType::NChar,
            TdsDataType::Char | TdsDataType::BigChar => ColumnType::Char,
            TdsDataType::NText => ColumnType::NText,
            TdsDataType::Text => ColumnType::Text,
            TdsDataType::Binary | TdsDataType::BigBinary => ColumnType::Binary,
            TdsDataType::VarBinary => ColumnType::VarBinary,
            TdsDataType::BigVarBinary => ColumnType::BigVarBin,
            TdsDataType::Image => ColumnType::Image,
            TdsDataType::Xml => ColumnType::Xml,
            TdsDataType::Json => ColumnType::Json,
            TdsDataType::Vector => ColumnType::Vector,
            TdsDataType::SsVariant => ColumnType::Ssvariant,
            TdsDataType::Udt => ColumnType::Udt,
            _ => ColumnType::Null,
        }
    }
}

impl ColumnType {
    fn from_metadata(meta: &mssql_tds::query::metadata::ColumnMetadata) -> Self {
        // A custom UDT can have the same name as a system spatial type.
        if meta.data_type == TdsDataType::Udt
            && let Some(info) = meta.type_info.udt_info()
            && info.schema_name().eq_ignore_ascii_case("sys")
        {
            if info.type_name().eq_ignore_ascii_case("geography") {
                return Self::Geography;
            }
            if info.type_name().eq_ignore_ascii_case("geometry") {
                return Self::Geometry;
            }
        }
        Self::from_tds_with_length(meta.data_type, meta.type_info.length)
    }

    /// Resolve the column type using both the TDS data type and the wire byte
    /// length. This is necessary for variable-width nullable types like `IntN`
    /// and `FltN` where the data type alone doesn't indicate the width.
    /// CLR UDTs resolve to [`Self::Udt`]; [`Column::from_tds`] uses the
    /// additional UDT identity metadata to recognize spatial types.
    pub fn from_tds_with_length(dt: TdsDataType, byte_length: usize) -> Self {
        match dt {
            TdsDataType::IntN => match byte_length {
                1 => ColumnType::Int1,
                2 => ColumnType::Int2,
                4 => ColumnType::Int4,
                8 => ColumnType::Int8,
                _ => ColumnType::Int4,
            },
            TdsDataType::FltN => match byte_length {
                4 => ColumnType::Float4,
                8 => ColumnType::Float8,
                _ => ColumnType::Float8,
            },
            TdsDataType::MoneyN => match byte_length {
                4 => ColumnType::Money4,
                8 => ColumnType::Money,
                _ => ColumnType::Money,
            },
            TdsDataType::DateTimeN => match byte_length {
                4 => ColumnType::Datetime4,
                _ => ColumnType::Datetime,
            },
            other => ColumnType::from(other),
        }
    }
}

/// SQL Server collation metadata exposed through a driver-owned type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Collation {
    /// Raw 32-bit collation info value.
    pub info: u32,
    /// LCID language identifier from the collation info.
    pub lcid_language_id: i32,
    /// Collation comparison flags.
    pub col_flags: u8,
    /// SQL Server sort ID.
    pub sort_id: u8,
}

impl From<mssql_tds::token::tokens::SqlCollation> for Collation {
    fn from(value: mssql_tds::token::tokens::SqlCollation) -> Self {
        Self {
            info: value.info,
            lcid_language_id: value.lcid_language_id,
            col_flags: value.col_flags,
            sort_id: value.sort_id,
        }
    }
}

/// Four-part source table name for a column, when supplied by SQL Server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MultiPartName {
    /// Server name portion.
    pub server_name: Option<String>,
    /// Catalog/database name portion.
    pub catalog_name: Option<String>,
    /// Schema name portion.
    pub schema_name: Option<String>,
    /// Table name portion.
    pub table_name: String,
}

/// Column metadata exposed to facade consumers.
#[derive(Debug, Clone)]
pub struct Column {
    /// Column name.
    pub(crate) name: String,
    /// Column data type.
    pub(crate) column_type: ColumnType,
    /// Whether the column is nullable.
    pub(crate) nullable: bool,
    /// Whether the column is an identity column.
    pub(crate) is_identity: bool,
    /// Whether the column is computed by SQL Server.
    pub(crate) is_computed: bool,
    /// Whether the column collation is case-sensitive.
    pub(crate) is_case_sensitive: bool,
    /// Whether the column is a sparse column set.
    pub(crate) is_sparse_column_set: bool,
    /// Whether the column is protected by Always Encrypted.
    pub(crate) is_encrypted: bool,
    /// Whether the column uses PLP (`max`) encoding.
    pub(crate) is_plp: bool,
    /// Wire byte length from TDS type info.
    pub(crate) byte_length: usize,
    /// Decimal/numeric/time scale, when supplied by SQL Server.
    pub(crate) scale: Option<u8>,
    /// Decimal/numeric or money precision, when supplied by SQL Server.
    pub(crate) precision: Option<u8>,
    /// String collation metadata, when supplied by SQL Server.
    pub(crate) collation: Option<Collation>,
    /// SQL Server user type ordinal.
    pub(crate) user_type: u32,
    /// Four-part source table name, when supplied by SQL Server.
    pub(crate) multi_part_name: Option<MultiPartName>,
}

impl Column {
    /// Returns the column name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the column data type.
    pub fn column_type(&self) -> ColumnType {
        self.column_type
    }

    /// Returns whether the column is nullable.
    pub fn nullable(&self) -> bool {
        self.nullable
    }

    /// Returns whether the column is an identity column.
    pub fn is_identity(&self) -> bool {
        self.is_identity
    }

    /// Returns whether the column is computed by SQL Server.
    pub fn is_computed(&self) -> bool {
        self.is_computed
    }

    /// Returns whether the column collation is case-sensitive.
    pub fn is_case_sensitive(&self) -> bool {
        self.is_case_sensitive
    }

    /// Returns whether the column is a sparse column set.
    pub fn is_sparse_column_set(&self) -> bool {
        self.is_sparse_column_set
    }

    /// Returns whether the column is protected by Always Encrypted.
    pub fn is_encrypted(&self) -> bool {
        self.is_encrypted
    }

    /// Returns whether the column uses partially length-prefixed (`max`) encoding.
    pub fn is_plp(&self) -> bool {
        self.is_plp
    }

    /// Returns the TDS wire byte length for the column.
    pub fn byte_length(&self) -> usize {
        self.byte_length
    }

    /// Returns decimal/numeric/time scale metadata, when available.
    pub fn scale(&self) -> Option<u8> {
        self.scale
    }

    /// Returns decimal/numeric or money precision metadata, when available.
    pub fn precision(&self) -> Option<u8> {
        self.precision
    }

    /// Returns string collation metadata, when available.
    pub fn collation(&self) -> Option<Collation> {
        self.collation
    }

    /// Returns the SQL Server user type ordinal.
    pub fn user_type(&self) -> u32 {
        self.user_type
    }

    /// Returns the source table four-part name, when available.
    pub fn multi_part_name(&self) -> Option<&MultiPartName> {
        self.multi_part_name.as_ref()
    }

    /// Declared column character length (e.g. `255` for `NVARCHAR(255)`).
    /// For unicode types (NVARCHAR/NCHAR/NTEXT) this is `byte_length / 2`.
    /// For `max` and non-string types, returns `None`.
    pub fn char_length(&self) -> Option<usize> {
        if self.is_plp {
            return None;
        }
        match self.column_type {
            ColumnType::NVarchar | ColumnType::NChar | ColumnType::NText => {
                Some(self.byte_length / 2)
            }
            ColumnType::Varchar | ColumnType::Char | ColumnType::Text => Some(self.byte_length),
            _ => None,
        }
    }

    /// Create a Column from mssql-tds ColumnMetadata.
    pub fn from_tds(meta: &mssql_tds::query::metadata::ColumnMetadata) -> Self {
        Column {
            name: meta.column_name.clone(),
            column_type: ColumnType::from_metadata(meta),
            nullable: meta.is_nullable(),
            is_identity: meta.is_identity(),
            is_computed: meta.is_computed(),
            is_case_sensitive: meta.is_case_sensitive(),
            is_sparse_column_set: meta.is_sparse_column_set(),
            is_encrypted: meta.is_encrypted(),
            is_plp: meta.is_plp(),
            byte_length: meta.type_info.length,
            scale: meta.get_scale(),
            precision: meta.get_precision(),
            collation: meta.get_collation().map(Collation::from),
            user_type: meta.user_type,
            multi_part_name: meta.multi_part_name.as_ref().map(|name| MultiPartName {
                server_name: name.server_name().map(str::to_owned),
                catalog_name: name.catalog_name().map(str::to_owned),
                schema_name: name.schema_name().map(str::to_owned),
                table_name: name.table_name().to_owned(),
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn test_column(name: &str, column_type: ColumnType, byte_length: usize) -> Self {
        Self {
            name: name.to_string(),
            column_type,
            nullable: true,
            is_identity: false,
            is_computed: false,
            is_case_sensitive: false,
            is_sparse_column_set: false,
            is_encrypted: false,
            is_plp: false,
            byte_length,
            scale: None,
            precision: None,
            collation: None,
            user_type: 0,
            multi_part_name: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mssql_tds::test_client_support::{int_columns, udt_column, udt_column_with_metadata};

    #[test]
    fn spatial_column_types_use_udt_identity() {
        for (schema, name, expected) in [
            ("sys", "geography", ColumnType::Geography),
            ("sys", "geometry", ColumnType::Geometry),
            ("SYS", "GEOGRAPHY", ColumnType::Geography),
            ("SYS", "GEOMETRY", ColumnType::Geometry),
            ("dbo", "geography", ColumnType::Udt),
            ("dbo", "geometry", ColumnType::Udt),
            ("sys", "hierarchyid", ColumnType::Udt),
            ("sys", "geography_extra", ColumnType::Udt),
            ("", "geometry", ColumnType::Udt),
        ] {
            let mut meta = udt_column_with_metadata(u16::MAX, "", schema, name, "assembly");
            // Classification must not depend on a database's user-type ordinals.
            meta.user_type = 12345;
            meta.column_name = "value".into();
            let column = Column::from_tds(&meta);
            assert_eq!(column.column_type(), expected, "{schema}.{name}");
            assert_eq!(column.name(), "value");
            assert_eq!(column.user_type(), 12345);
            assert!(column.nullable());
            assert!(column.is_plp());
            assert_eq!(column.byte_length(), usize::from(u16::MAX));
            assert_eq!(column.char_length(), None);
        }
    }

    #[test]
    fn spatial_column_type_requires_udt_metadata() {
        assert_eq!(ColumnType::from(TdsDataType::Udt), ColumnType::Udt);
        assert_eq!(
            ColumnType::from_tds_with_length(TdsDataType::Udt, 65535),
            ColumnType::Udt
        );
        assert_eq!(
            Column::from_tds(&udt_column(u16::MAX)).column_type(),
            ColumnType::Udt
        );
        let mut meta = int_columns(1).remove(0);
        meta.column_name = "geography".into();
        assert_eq!(Column::from_tds(&meta).column_type(), ColumnType::Int4);
    }

    #[test]
    fn test_column_metadata_accessors() {
        let column = Column {
            name: "id".to_string(),
            column_type: ColumnType::Int4,
            nullable: false,
            is_identity: true,
            is_computed: true,
            is_case_sensitive: true,
            is_sparse_column_set: true,
            is_encrypted: true,
            is_plp: true,
            byte_length: 4,
            scale: Some(2),
            precision: Some(9),
            collation: Some(Collation {
                info: 0x0010_0409,
                lcid_language_id: 1033,
                col_flags: 1,
                sort_id: 52,
            }),
            user_type: 7,
            multi_part_name: Some(MultiPartName {
                server_name: Some("server".to_string()),
                catalog_name: Some("db".to_string()),
                schema_name: Some("dbo".to_string()),
                table_name: "users".to_string(),
            }),
        };

        assert_eq!(column.name(), "id");
        assert_eq!(column.column_type(), ColumnType::Int4);
        assert!(!column.nullable());
        assert!(column.is_identity());
        assert!(column.is_computed());
        assert!(column.is_case_sensitive());
        assert!(column.is_sparse_column_set());
        assert!(column.is_encrypted());
        assert!(column.is_plp());
        assert_eq!(column.byte_length(), 4);
        assert_eq!(column.scale(), Some(2));
        assert_eq!(column.precision(), Some(9));
        assert_eq!(
            column.collation().expect("column has a collation").sort_id,
            52
        );
        assert_eq!(column.user_type(), 7);
        assert_eq!(
            column
                .multi_part_name()
                .expect("column has a table name")
                .table_name,
            "users"
        );
    }

    #[test]
    fn from_tds_preserves_precision_and_source_name() {
        let mut meta = int_columns(1).remove(0);
        let column = Column::from_tds(&meta);
        assert_eq!(column.precision(), None);
        assert_eq!(column.multi_part_name(), None);

        meta.data_type = TdsDataType::DecimalN;
        meta.type_info = mssql_tds::datatypes::sqldatatypes::TypeInfo::var_len_precision_scale(
            TdsDataType::DecimalN,
            9,
            18,
            4,
        )
        .expect("decimal type info");
        meta.multi_part_name = Some(Default::default());
        let column = Column::from_tds(&meta);
        assert_eq!(column.precision(), Some(18));
        assert_eq!(column.scale(), Some(4));
        assert_eq!(
            column.multi_part_name(),
            Some(&MultiPartName {
                server_name: None,
                catalog_name: None,
                schema_name: None,
                table_name: String::new(),
            })
        );
    }

    #[test]
    fn test_char_length() {
        assert_eq!(
            Column::test_column("n", ColumnType::NVarchar, 510).char_length(),
            Some(255)
        );
        assert_eq!(
            Column::test_column("c", ColumnType::Varchar, 255).char_length(),
            Some(255)
        );
        assert_eq!(
            Column::test_column("i", ColumnType::Int4, 4).char_length(),
            None
        );
    }

    #[test]
    fn variable_width_metadata_preserves_binary_types_and_max_lengths() {
        use mssql_tds::datatypes::sqldatatypes::TypeInfo;

        assert_eq!(
            ColumnType::from(TdsDataType::VarBinary),
            ColumnType::VarBinary
        );
        for length in [32, usize::from(u16::MAX)] {
            let mut meta = int_columns(1).remove(0);
            meta.data_type = TdsDataType::BigVarBinary;
            meta.type_info = if length == usize::from(u16::MAX) {
                TypeInfo::partial_len(meta.data_type, length, None).expect("max binary metadata")
            } else {
                TypeInfo::var_len(meta.data_type, length).expect("binary metadata")
            };
            let column = Column::from_tds(&meta);
            assert_eq!(column.column_type(), ColumnType::BigVarBin);
            assert_eq!(column.is_plp(), length == usize::from(u16::MAX));
        }
        for (tds_type, expected_length) in
            [(TdsDataType::BigVarChar, 32), (TdsDataType::NVarChar, 16)]
        {
            let mut meta = int_columns(1).remove(0);
            meta.data_type = tds_type;
            meta.type_info = TypeInfo::var_len_string(tds_type, 32, None).expect("string metadata");
            assert_eq!(Column::from_tds(&meta).char_length(), Some(expected_length));
            meta.type_info = TypeInfo::partial_len(tds_type, usize::from(u16::MAX), None)
                .expect("max string metadata");
            let column = Column::from_tds(&meta);
            assert!(column.is_plp());
            assert_eq!(column.char_length(), None);
        }
    }

    #[test]
    fn test_collation_from_tds() {
        let tds = mssql_tds::token::tokens::SqlCollation {
            info: 0x0010_0409,
            lcid_language_id: 1033,
            col_flags: 1,
            sort_id: 52,
        };
        let collation = Collation::from(tds);

        assert_eq!(collation.info, 0x0010_0409);
        assert_eq!(collation.lcid_language_id, 1033);
        assert_eq!(collation.col_flags, 1);
        assert_eq!(collation.sort_id, 52);
    }

    #[test]
    fn test_column_type_from_tds() {
        assert_eq!(ColumnType::from(TdsDataType::Int4), ColumnType::Int4);
        assert_eq!(ColumnType::from(TdsDataType::Bit), ColumnType::Bit);
        assert_eq!(
            ColumnType::from(TdsDataType::NVarChar),
            ColumnType::NVarchar
        );
        assert_eq!(
            ColumnType::from(TdsDataType::DateTime),
            ColumnType::Datetime
        );
        assert_eq!(ColumnType::from(TdsDataType::Guid), ColumnType::Guid);
        assert_eq!(
            ColumnType::from(TdsDataType::DecimalN),
            ColumnType::Decimaln
        );
        assert_eq!(ColumnType::from(TdsDataType::Image), ColumnType::Image);
    }

    #[test]
    fn fixed_width_wire_types_preserve_column_types() {
        use mssql_tds::datatypes::sqldatatypes::TypeInfo;

        let cases = [
            (
                TdsDataType::BigChar,
                TypeInfo::var_len_string(TdsDataType::BigChar, 10, None).expect("type info"),
                ColumnType::Char,
            ),
            (
                TdsDataType::BigBinary,
                TypeInfo::var_len(TdsDataType::BigBinary, 10).expect("type info"),
                ColumnType::Binary,
            ),
            (
                TdsDataType::Money4,
                TypeInfo::fixed_len(TdsDataType::Money4).expect("type info"),
                ColumnType::Money4,
            ),
            (
                TdsDataType::DateTimeN,
                TypeInfo::var_len(TdsDataType::DateTimeN, 4).expect("type info"),
                ColumnType::Datetime4,
            ),
            (
                TdsDataType::DateTimeN,
                TypeInfo::var_len(TdsDataType::DateTimeN, 8).expect("type info"),
                ColumnType::Datetime,
            ),
        ];
        for (tds_type, info, expected) in cases {
            let mut meta = int_columns(1).remove(0);
            meta.data_type = tds_type;
            meta.type_info = info;
            assert_eq!(Column::from_tds(&meta).column_type(), expected);
        }
    }

    #[test]
    fn test_column_type_nullable_variants() {
        assert_eq!(ColumnType::from(TdsDataType::BitN), ColumnType::Bit);
        assert_eq!(ColumnType::from(TdsDataType::FltN), ColumnType::Float8);
        // IntN via From defaults to Int4 (no length info available)
        assert_eq!(ColumnType::from(TdsDataType::IntN), ColumnType::Int4);
    }

    #[test]
    fn test_intn_resolved_by_byte_length() {
        assert_eq!(
            ColumnType::from_tds_with_length(TdsDataType::IntN, 1),
            ColumnType::Int1
        );
        assert_eq!(
            ColumnType::from_tds_with_length(TdsDataType::IntN, 2),
            ColumnType::Int2
        );
        assert_eq!(
            ColumnType::from_tds_with_length(TdsDataType::IntN, 4),
            ColumnType::Int4
        );
        assert_eq!(
            ColumnType::from_tds_with_length(TdsDataType::IntN, 8),
            ColumnType::Int8
        );
    }

    #[test]
    fn test_fltn_resolved_by_byte_length() {
        assert_eq!(
            ColumnType::from_tds_with_length(TdsDataType::FltN, 4),
            ColumnType::Float4
        );
        assert_eq!(
            ColumnType::from_tds_with_length(TdsDataType::FltN, 8),
            ColumnType::Float8
        );
    }

    #[test]
    fn test_moneyn_resolved_by_byte_length() {
        assert_eq!(
            ColumnType::from_tds_with_length(TdsDataType::MoneyN, 4),
            ColumnType::Money4
        );
        assert_eq!(
            ColumnType::from_tds_with_length(TdsDataType::MoneyN, 8),
            ColumnType::Money
        );
    }

    #[test]
    fn test_from_tds_with_length_falls_through_for_fixed_types() {
        // Non-variable types should pass through to From<TdsDataType>
        assert_eq!(
            ColumnType::from_tds_with_length(TdsDataType::Int4, 4),
            ColumnType::Int4
        );
        assert_eq!(
            ColumnType::from_tds_with_length(TdsDataType::Bit, 1),
            ColumnType::Bit
        );
        assert_eq!(
            ColumnType::from_tds_with_length(TdsDataType::NVarChar, 100),
            ColumnType::NVarchar
        );
    }
}
