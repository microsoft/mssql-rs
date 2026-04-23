// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Bulk copy column metadata structures for BulkCopy operations.
//!
//! This module provides the metadata structures needed to properly transmit
//! column information during bulk copy operations, matching the .NET SqlBulkCopy
//! implementation's metadata handling.

use crate::{query::metadata::ColumnMetadata, token::tokens::SqlCollation};
use tracing::warn;

/// Newtype wrapper for SQL Server's system_type_id values.
///
/// This type represents the internal type identifiers stored in SQL Server's
/// catalog tables (sys.columns.system_type_id, sys.types.system_type_id).
/// These are different from TDS protocol type bytes used during wire transmission.
///
/// Using a newtype makes the conversion more explicit and self-documenting:
/// - `SqlDbType::try_from(SystemTypeId(56))?` is clearer than `SqlDbType::try_from(56u8)?`
/// - It prevents confusion between system_type_id values and TDS type bytes
/// - Allows for future TryFrom implementations without ambiguity
///
/// # Example
///
/// ```rust,ignore
/// use mssql_tds::datatypes::bulk_copy_metadata::{SqlDbType, SystemTypeId};
///
/// // Convert from sys.columns.system_type_id (56 = int)
/// let sql_type = SqlDbType::try_from(SystemTypeId(56))?;
/// assert_eq!(sql_type, SqlDbType::Int);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SystemTypeId(pub u8);

/// SQL Database types supported in bulk copy operations.
///
/// This enum represents the SQL Server data types that can be used in BulkCopy.
/// It aligns with SQL Server's type system and TDS protocol requirements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlDbType {
    /// 64-bit signed integer (`bigint`).
    BigInt,
    /// 32-bit signed integer (`int`).
    Int,
    /// 16-bit signed integer (`smallint`).
    SmallInt,
    /// 8-bit unsigned integer (`tinyint`).
    TinyInt,
    /// Boolean (`bit`).
    Bit,

    /// Double-precision IEEE 754 (`float`).
    Float,
    /// Single-precision IEEE 754 (`real`).
    Real,

    /// Fixed-precision numeric (`decimal`).
    Decimal,
    /// Fixed-precision numeric (`numeric`). Functionally identical to `Decimal`.
    Numeric,
    /// Currency with 4 decimal places, 8 bytes (`money`).
    Money,
    /// Currency with 4 decimal places, 4 bytes (`smallmoney`).
    SmallMoney,

    /// Date without time component (`date`).
    Date,
    /// Legacy datetime, ~3.33 ms precision (`datetime`).
    DateTime,
    /// High-precision datetime, configurable fractional seconds (`datetime2`).
    DateTime2,
    /// `datetime2` with UTC offset (`datetimeoffset`).
    DateTimeOffset,
    /// Legacy datetime, 1-minute precision (`smalldatetime`).
    SmallDateTime,
    /// Time without date component (`time`).
    Time,

    /// Fixed-length non-Unicode string (`char`).
    Char,
    /// Variable-length non-Unicode string (`varchar`).
    VarChar,
    /// Legacy LOB non-Unicode text — prefer `varchar(max)` (`text`).
    Text,
    /// Fixed-length Unicode string (`nchar`).
    NChar,
    /// Variable-length Unicode string (`nvarchar`).
    NVarChar,
    /// Legacy LOB Unicode text — prefer `nvarchar(max)` (`ntext`).
    NText,

    /// Fixed-length binary data (`binary`).
    Binary,
    /// Variable-length binary data (`varbinary`).
    VarBinary,
    /// Legacy LOB binary — prefer `varbinary(max)` (`image`).
    Image,

    /// 16-byte GUID (`uniqueidentifier`).
    UniqueIdentifier,
    /// XML data (`xml`). Transmitted as `nvarchar(max)` during bulk copy.
    Xml,
    /// Polymorphic type (`sql_variant`).
    Variant,
    /// CLR user-defined type (`udt`).
    Udt,

    /// JSON data (`json`). Transmitted as `nvarchar(max)` during bulk copy.
    Json,

    /// Fixed-dimensional vector (SQL Server 2025+).
    Vector,
}

impl SqlDbType {
    /// Map SqlDbType to TDS protocol type byte for bulk copy.
    ///
    /// Returns the TDS type identifier used in the COLMETADATA token.
    /// For bulk copy, we use nullable variant types (e.g., INTN instead of INT4)
    /// to support nullable columns, matching .NET SqlBulkCopy behavior.
    /// Reference: TDS protocol specification and .NET MetaType.NullableType
    pub fn to_tds_type(&self) -> u8 {
        match self {
            // Integer types - use INTN (0x26) for nullable int types
            SqlDbType::TinyInt => 0x26,  // TdsDataType::IntN (length 1)
            SqlDbType::SmallInt => 0x26, // TdsDataType::IntN (length 2)
            SqlDbType::Int => 0x26,      // TdsDataType::IntN (length 4)
            SqlDbType::BigInt => 0x26,   // TdsDataType::IntN (length 8)
            SqlDbType::Bit => 0x68,      // TdsDataType::BitN

            // Floating point types - use FLTN (0x6D) for nullable float types
            SqlDbType::Real => 0x6D,  // TdsDataType::FltN (length 4)
            SqlDbType::Float => 0x6D, // TdsDataType::FltN (length 8)

            // Exact numeric types
            SqlDbType::Decimal => 0x6A,    // TdsDataType::DecimalN
            SqlDbType::Numeric => 0x6C,    // TdsDataType::NumericN
            SqlDbType::Money => 0x6E,      // TdsDataType::MoneyN (length 8)
            SqlDbType::SmallMoney => 0x6E, // TdsDataType::MoneyN (length 4)

            // Date and time types
            SqlDbType::Date => 0x28,           // TdsDataType::DateN
            SqlDbType::Time => 0x29,           // TdsDataType::TimeN
            SqlDbType::DateTime => 0x6F,       // TdsDataType::DateTimeN (length 8)
            SqlDbType::DateTime2 => 0x2A,      // TdsDataType::DateTime2N
            SqlDbType::DateTimeOffset => 0x2B, // TdsDataType::DateTimeOffsetN
            SqlDbType::SmallDateTime => 0x6F,  // TdsDataType::DateTimeN (length 4)

            // Character types
            SqlDbType::Char => 0xAF,     // TdsDataType::BigChar
            SqlDbType::VarChar => 0xA7,  // TdsDataType::BigVarChar
            SqlDbType::Text => 0x23,     // TdsDataType::Text
            SqlDbType::NChar => 0xEF,    // TdsDataType::NChar
            SqlDbType::NVarChar => 0xE7, // TdsDataType::NVarChar
            SqlDbType::NText => 0x63,    // TdsDataType::NText

            // Binary types
            SqlDbType::Binary => 0xAD,    // TdsDataType::BigBinary
            SqlDbType::VarBinary => 0xA5, // TdsDataType::BigVarBinary
            SqlDbType::Image => 0x22,     // TdsDataType::Image

            // Other types
            SqlDbType::UniqueIdentifier => 0x24, // TdsDataType::Guid
            SqlDbType::Xml => 0xF1,              // TdsDataType::Xml
            SqlDbType::Json => 0xF4,             // TdsDataType::Json
            SqlDbType::Variant => 0x62,          // TdsDataType::SsVariant
            SqlDbType::Udt => 0xF0,              // TdsDataType::Udt
            SqlDbType::Vector => 0xF5,           // TdsDataType::Vector
        }
    }

    /// Map SqlDbType to TDS protocol type byte for bulk copy operations.
    ///
    /// This method returns the TDS type that should actually be used when sending
    /// data via bulk copy. For most types, this is the same as `to_tds_type()`,
    /// but some types require special handling:
    ///
    /// - XML: Returns 0xE7 (NVarChar) because the TDS spec requires XML data to be
    ///   sent as NVARCHAR(MAX) in bulk copy operations. XML data must be sent as
    ///   NVARCHAR(MAX) with UTF-16LE encoding. Sending as XMLTYPE (0xF1) causes
    ///   "Invalid column type from bcp client" errors.
    /// - JSON: Returns 0xE7 (NVarChar) because SQL Server doesn't support sending
    ///   JSON type directly in bulk copy operations. JSON data must be sent as
    ///   NVARCHAR(MAX) with UTF-16LE encoding.
    ///
    /// This makes the intention explicit in code: XML/JSON are their respective types,
    /// but for bulk copy purposes we transmit them as NVARCHAR.
    pub fn to_bulk_copy_tds_type(&self) -> u8 {
        match self {
            // XML must be sent as NVARCHAR(MAX) in bulk copy
            SqlDbType::Xml => 0xE7, // TdsDataType::NVarChar - TDS spec requirement
            // JSON must be sent as NVARCHAR(MAX) in bulk copy
            SqlDbType::Json => 0xE7, // TdsDataType::NVarChar - bulk copy workaround
            // All other types use their standard TDS type
            _ => self.to_tds_type(),
        }
    }

    /// Convert SqlDbType to TDS type byte using fixed-length types (non-nullable).
    ///
    /// Use this for NOT NULL columns to generate more compact wire format.
    /// For nullable columns, use `to_tds_type()` instead.
    pub fn to_tds_type_fixed(&self) -> u8 {
        match self {
            // Integer types - use fixed-length types
            SqlDbType::TinyInt => 0x30,  // TdsDataType::Int1
            SqlDbType::SmallInt => 0x34, // TdsDataType::Int2
            SqlDbType::Int => 0x38,      // TdsDataType::Int4
            SqlDbType::BigInt => 0x7F,   // TdsDataType::Int8
            SqlDbType::Bit => 0x32,      // TdsDataType::Bit

            // Floating point types - use fixed-length types
            SqlDbType::Real => 0x3B,  // TdsDataType::Flt4
            SqlDbType::Float => 0x3E, // TdsDataType::Flt8

            // For other types, use the nullable variant (they don't have fixed versions)
            _ => self.to_tds_type(),
        }
    }
}

/// Convert SQL Server `SystemTypeId` to `SqlDbType`.
///
/// This mapping is based on the sys.types catalog view in SQL Server.
/// The `SystemTypeId` wraps SQL Server's internal type identifiers (system_type_id)
/// stored in catalog tables, which are different from the TDS protocol type bytes
/// used during data transmission.
///
/// Using the newtype `SystemTypeId` instead of raw `u8` makes the conversion more
/// explicit and self-documenting, preventing confusion with TDS type bytes.
///
/// Reference: <https://docs.microsoft.com/en-us/sql/relational-databases/system-catalog-views/sys-types-transact-sql>
///
/// # Example
///
/// ```rust,ignore
/// use mssql_tds::datatypes::bulk_copy_metadata::{SqlDbType, SystemTypeId};
///
/// // From sys.columns.system_type_id
/// let sql_type = SqlDbType::try_from(SystemTypeId(56))?; // 56 = int
/// assert_eq!(sql_type, SqlDbType::Int);
/// ```
impl TryFrom<SystemTypeId> for SqlDbType {
    type Error = crate::error::Error;

    fn try_from(id: SystemTypeId) -> Result<Self, Self::Error> {
        use crate::error::Error;

        match id.0 {
            // Exact numeric types
            48 => Ok(SqlDbType::TinyInt),     // tinyint
            52 => Ok(SqlDbType::SmallInt),    // smallint
            56 => Ok(SqlDbType::Int),         // int
            127 => Ok(SqlDbType::BigInt),     // bigint
            106 => Ok(SqlDbType::Decimal),    // decimal
            108 => Ok(SqlDbType::Numeric),    // numeric
            122 => Ok(SqlDbType::SmallMoney), // smallmoney
            60 => Ok(SqlDbType::Money),       // money
            104 => Ok(SqlDbType::Bit),        // bit

            // Approximate numeric types
            59 => Ok(SqlDbType::Real),  // real
            62 => Ok(SqlDbType::Float), // float

            // Date and time types
            40 => Ok(SqlDbType::Date),           // date
            41 => Ok(SqlDbType::Time),           // time
            42 => Ok(SqlDbType::DateTime2),      // datetime2
            43 => Ok(SqlDbType::DateTimeOffset), // datetimeoffset
            58 => Ok(SqlDbType::SmallDateTime),  // smalldatetime
            61 => Ok(SqlDbType::DateTime),       // datetime

            // Character strings
            167 => Ok(SqlDbType::VarChar), // varchar
            175 => Ok(SqlDbType::Char),    // char
            35 => Ok(SqlDbType::Text),     // text

            // Unicode character strings
            231 => Ok(SqlDbType::NVarChar), // nvarchar
            239 => Ok(SqlDbType::NChar),    // nchar
            99 => Ok(SqlDbType::NText),     // ntext

            // Binary strings
            165 => Ok(SqlDbType::VarBinary), // varbinary
            173 => Ok(SqlDbType::Binary),    // binary
            34 => Ok(SqlDbType::Image),      // image

            // Other types
            36 => Ok(SqlDbType::UniqueIdentifier), // uniqueidentifier
            241 => Ok(SqlDbType::Xml),             // xml

            // Unsupported or unknown types
            _ => Err(Error::UsageError(format!(
                "Unsupported system_type_id: {}",
                id.0
            ))),
        }
    }
}

/// Encoding types for character data.
///
/// Represents different character encodings that can be used for string columns,
/// determined by the column's collation.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum EncodingType {
    /// UTF-8 encoding
    #[default]
    Utf8,
    /// UTF-16 Little Endian (used for NCHAR/NVARCHAR)
    Utf16Le,
    /// Latin-1 (ISO-8859-1)
    Latin1,
    /// Windows code page encoding
    CodePage(u16),
}

impl EncodingType {
    /// Encode a Rust string to bytes using this encoding.
    pub fn encode(&self, s: &str) -> Vec<u8> {
        match self {
            EncodingType::Utf8 => s.as_bytes().to_vec(),
            EncodingType::Utf16Le => s.encode_utf16().flat_map(|c| c.to_le_bytes()).collect(),
            EncodingType::Latin1 => {
                // Latin-1 is the first 256 Unicode code points
                s.chars()
                    .map(|c| {
                        let code = c as u32;
                        if code < 256 {
                            code as u8
                        } else {
                            b'?' // Replacement character
                        }
                    })
                    .collect()
            }
            EncodingType::CodePage(cp) => {
                // For now, fallback to UTF-8 for non-Latin1 code pages
                // A full implementation would use encoding_rs or similar
                warn!("Code page {} not fully supported, using UTF-8", cp);
                s.as_bytes().to_vec()
            }
        }
    }

    /// Calculate the byte length of a string in this encoding.
    pub fn byte_length(&self, s: &str) -> usize {
        match self {
            EncodingType::Utf8 => s.len(),
            EncodingType::Utf16Le => s.len() * 2,
            EncodingType::Latin1 => s.len(),
            EncodingType::CodePage(_) => s.len(), // Approximation
        }
    }

    /// Get the encoding for NCHAR/NVARCHAR types (always UTF-16LE).
    pub fn unicode() -> Self {
        EncodingType::Utf16Le
    }

    /// Get the default encoding for CHAR/VARCHAR types (UTF-8).
    pub fn default_ansi() -> Self {
        EncodingType::Utf8
    }
}

/// Type length specification for SQL types.
///
/// Different SQL types have different length semantics:
/// - Fixed: Types with a fixed size (e.g., INT, BIGINT)
/// - Variable: Variable-length types with a maximum (e.g., VARCHAR(100))
/// - Plp: Partial Length Prefix types (MAX types: VARCHAR(MAX), VARBINARY(MAX))
/// - Unknown: For streaming data where length is not known in advance
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeLength {
    /// Fixed-length type (e.g., INT is always 4 bytes)
    Fixed(i32),
    /// Variable-length type with maximum (e.g., VARCHAR(100))
    Variable(i32),
    /// Partial Length Prefix (MAX types)
    Plp,
    /// Unknown length (for streaming)
    Unknown,
}

impl TypeLength {
    /// Check if this is a PLP (MAX) type.
    pub fn is_plp(&self) -> bool {
        matches!(self, TypeLength::Plp)
    }

    /// Check if this is a fixed-length type.
    pub fn is_fixed(&self) -> bool {
        matches!(self, TypeLength::Fixed(_))
    }

    /// Get the maximum length, if applicable.
    pub fn max_length(&self) -> Option<i32> {
        match self {
            TypeLength::Fixed(len) | TypeLength::Variable(len) => Some(*len),
            TypeLength::Plp | TypeLength::Unknown => None,
        }
    }
}

/// Column metadata for bulk copy operations.
///
/// This structure contains all the metadata needed to properly serialize
/// a column's data in the TDS bulk load protocol. It closely mirrors the
/// .NET SqlBulkCopy implementation's `_SqlMetaData` structure.
///
/// # Example
///
/// ```rust,ignore
/// use mssql_tds::datatypes::bulk_copy_metadata::{BulkCopyColumnMetadata, SqlDbType, TypeLength};
///
/// let metadata = BulkCopyColumnMetadata {
///     column_name: "id".to_string(),
///     sql_type: SqlDbType::Int,
///     tds_type: 0x38, // SQLINT4
///     length: 4,
///     length_type: TypeLength::Fixed(4),
///     precision: 0,
///     scale: 0,
///     collation: None,
///     encoding: None,
///     is_nullable: false,
///     is_identity: true,
///     is_encrypted: false,
///     table_name: None,
/// };
/// ```
#[derive(Debug, Clone)]
pub struct BulkCopyColumnMetadata {
    /// Column name
    pub column_name: String,

    /// SQL data type
    pub sql_type: SqlDbType,

    /// TDS wire protocol type byte
    pub tds_type: u8,

    /// Maximum length for the column
    pub length: i32,

    /// Length type classification
    pub length_type: TypeLength,

    /// Precision (for Decimal/Numeric types)
    pub precision: u8,

    /// Scale (for Decimal/Numeric/Time/DateTime2/DateTimeOffset types)
    pub scale: u8,

    /// Collation information (for character types)
    pub collation: Option<SqlCollation>,

    /// Collation name (e.g., "SQL_Latin1_General_CP1_CI_AS")
    /// This is the collation name retrieved from sp_tablecollations_100
    /// and used in the INSERT BULK SQL command.
    pub collation_name: Option<String>,

    /// Character encoding (for character types)
    pub encoding: Option<EncodingType>,

    /// Whether the column accepts NULL values
    pub is_nullable: bool,

    /// Whether the column is an identity column
    pub is_identity: bool,

    /// Whether the column is encrypted (Always Encrypted)
    pub is_encrypted: bool,

    /// Table name (for long/LOB types in some TDS versions)
    pub table_name: Option<String>,
}

impl BulkCopyColumnMetadata {
    /// Create a new metadata instance with required fields.
    pub fn new(column_name: impl Into<String>, sql_type: SqlDbType, tds_type: u8) -> Self {
        Self {
            column_name: column_name.into(),
            sql_type,
            tds_type,
            length: 0,
            length_type: TypeLength::Fixed(0),
            precision: 0,
            scale: 0,
            collation: None,
            collation_name: None,
            encoding: None,
            is_nullable: true,
            is_identity: false,
            is_encrypted: false,
            table_name: None,
        }
    }

    /// Set the column length.
    pub fn with_length(mut self, length: i32, length_type: TypeLength) -> Self {
        self.length = length;
        self.length_type = length_type;
        self
    }

    /// Set precision and scale (for Decimal/Numeric).
    pub fn with_precision_scale(mut self, precision: u8, scale: u8) -> Self {
        self.precision = precision;
        self.scale = scale;
        self
    }

    /// Set scale only (for Time/DateTime2/DateTimeOffset).
    pub fn with_scale(mut self, scale: u8) -> Self {
        self.scale = scale;
        self
    }

    /// Set collation (for character types).
    pub fn with_collation(mut self, collation: SqlCollation) -> Self {
        self.collation = Some(collation);
        self
    }

    /// Set collation name (for character types).
    /// This is used in the INSERT BULK SQL command.
    pub fn with_collation_name(mut self, collation_name: impl Into<String>) -> Self {
        self.collation_name = Some(collation_name.into());
        self
    }

    /// Set encoding (for character types).
    pub fn with_encoding(mut self, encoding: EncodingType) -> Self {
        self.encoding = Some(encoding);
        self
    }

    /// Set nullable flag.
    pub fn with_nullable(mut self, nullable: bool) -> Self {
        self.is_nullable = nullable;
        self
    }

    /// Set identity flag.
    pub fn with_identity(mut self, identity: bool) -> Self {
        self.is_identity = identity;
        self
    }

    /// Set encrypted flag.
    pub fn with_encrypted(mut self, encrypted: bool) -> Self {
        self.is_encrypted = encrypted;
        self
    }

    /// Set table name (for LOB types).
    pub fn with_table_name(mut self, table_name: impl Into<String>) -> Self {
        self.table_name = Some(table_name.into());
        self
    }

    /// Check if this is a character type that needs collation.
    pub fn needs_collation(&self) -> bool {
        matches!(
            self.sql_type,
            SqlDbType::Char
                | SqlDbType::VarChar
                | SqlDbType::Text
                | SqlDbType::NChar
                | SqlDbType::NVarChar
                | SqlDbType::NText
        )
    }

    /// Check if this is a numeric type that needs precision/scale.
    pub fn needs_precision_scale(&self) -> bool {
        matches!(self.sql_type, SqlDbType::Decimal | SqlDbType::Numeric)
    }

    /// Check if this is a PLP (MAX) type.
    pub fn is_plp(&self) -> bool {
        self.length_type.is_plp()
    }

    /// Check if this is a long type (legacy LOB types).
    pub fn is_long(&self) -> bool {
        matches!(
            self.sql_type,
            SqlDbType::Text | SqlDbType::NText | SqlDbType::Image
        )
    }

    /// Get the SQL type definition string for this column.
    ///
    /// This generates a SQL type definition string similar to what's used in
    /// CREATE TABLE statements and sp_executesql parameter lists.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// let meta = BulkCopyColumnMetadata::new("id", SqlDbType::Int, 0x38);
    /// assert_eq!(meta.get_sql_type_definition().unwrap(), "int");
    ///
    /// let meta = BulkCopyColumnMetadata::new("name", SqlDbType::NVarChar, 0xE7)
    ///     .with_length(100, TypeLength::Variable(100));
    /// assert_eq!(meta.get_sql_type_definition().unwrap(), "nvarchar(100)");
    /// ```
    pub fn get_sql_type_definition(&self) -> crate::core::TdsResult<String> {
        Ok(match self.sql_type {
            SqlDbType::Int => "int".to_string(),
            SqlDbType::BigInt => "bigint".to_string(),
            SqlDbType::SmallInt => "smallint".to_string(),
            SqlDbType::TinyInt => "tinyint".to_string(),
            SqlDbType::Bit => "bit".to_string(),
            SqlDbType::Decimal | SqlDbType::Numeric => {
                format!("decimal({}, {})", self.precision, self.scale)
            }
            SqlDbType::Float => "float".to_string(),
            SqlDbType::Real => "real".to_string(),
            SqlDbType::Money => "money".to_string(),
            SqlDbType::SmallMoney => "smallmoney".to_string(),
            SqlDbType::NVarChar => {
                if self.is_plp() {
                    "nvarchar(max)".to_string()
                } else {
                    // CRITICAL: self.length is in BYTES for NVARCHAR
                    // T-SQL requires CHARACTER count, so divide by 2
                    format!("nvarchar({})", self.length / 2)
                }
            }
            SqlDbType::NChar => {
                // CRITICAL: self.length is in BYTES for NCHAR
                // T-SQL requires CHARACTER count, so divide by 2
                format!("nchar({})", self.length / 2)
            }
            SqlDbType::VarChar => {
                if self.is_plp() {
                    "varchar(max)".to_string()
                } else {
                    format!("varchar({})", self.length)
                }
            }
            SqlDbType::Char => format!("char({})", self.length),
            SqlDbType::VarBinary => {
                if self.is_plp() {
                    "varbinary(max)".to_string()
                } else {
                    format!("varbinary({})", self.length)
                }
            }
            SqlDbType::Binary => format!("binary({})", self.length),
            SqlDbType::UniqueIdentifier => "uniqueidentifier".to_string(),
            SqlDbType::DateTime => "datetime".to_string(),
            SqlDbType::SmallDateTime => "smalldatetime".to_string(),
            SqlDbType::Date => "date".to_string(),
            SqlDbType::Time => format!("time({})", self.scale),
            SqlDbType::DateTime2 => format!("datetime2({})", self.scale),
            SqlDbType::DateTimeOffset => format!("datetimeoffset({})", self.scale),
            SqlDbType::Text => "text".to_string(),
            SqlDbType::NText => "ntext".to_string(),
            SqlDbType::Image => "image".to_string(),
            // XML must be sent as NVARCHAR(MAX) in bulk copy, but we report it as XML type
            // in INSERT BULK statement. This is similar to ODBC bulk copy behavior.
            SqlDbType::Xml => "xml".to_string(),
            SqlDbType::Udt => format!("varbinary({})", self.length),
            SqlDbType::Variant => "sql_variant".to_string(),
            SqlDbType::Json => "nvarchar(max)".to_string(),
            SqlDbType::Vector => {
                let dims = self.vector_dimensions()?;
                format!("vector({})", dims)
            }
        })
    }

    /// Compute VECTOR dimensions from total `length` and base type encoded in `scale`.
    ///
    /// Returns an error if the column is not a VECTOR, if `length` is smaller
    /// than the header size, or if the payload is not divisible by the element size.
    pub fn vector_dimensions(&self) -> crate::core::TdsResult<usize> {
        use crate::datatypes::sqldatatypes::{VECTOR_HEADER_SIZE, VectorBaseType};

        if self.sql_type != SqlDbType::Vector {
            return Err(crate::error::Error::UsageError(format!(
                "Column '{}' is not a VECTOR type",
                self.column_name
            )));
        }

        let base_type = VectorBaseType::try_from(self.scale)?;
        let elem_size = base_type.element_size_bytes();
        let length = self.length as usize;

        if length < VECTOR_HEADER_SIZE {
            return Err(crate::error::Error::UsageError(format!(
                "Invalid VECTOR metadata length ({}). Must be >= header size {}.",
                length, VECTOR_HEADER_SIZE
            )));
        }

        let payload_bytes = length - VECTOR_HEADER_SIZE;
        if !payload_bytes.is_multiple_of(elem_size) {
            return Err(crate::error::Error::UsageError(format!(
                "Invalid VECTOR metadata length: payload {} not divisible by element size {}",
                payload_bytes, elem_size
            )));
        }

        Ok(payload_bytes / elem_size)
    }
}

impl Default for BulkCopyColumnMetadata {
    fn default() -> Self {
        Self {
            column_name: String::new(),
            sql_type: SqlDbType::Int,
            tds_type: 0x38, // SQLINT4
            length: 4,
            length_type: TypeLength::Fixed(4),
            precision: 0,
            scale: 0,
            collation: None,
            collation_name: None,
            encoding: None,
            is_nullable: true,
            is_identity: false,
            is_encrypted: false,
            table_name: None,
        }
    }
}

/// Convert from a TDS ColumnMetadata (from COLMETADATA token) to BulkCopyColumnMetadata.
///
/// This function extracts the TDS type and other metadata directly from the server's
/// COLMETADATA response, ensuring we use the exact types that SQL Server expects.
impl From<&ColumnMetadata> for BulkCopyColumnMetadata {
    fn from(col: &crate::query::metadata::ColumnMetadata) -> Self {
        use crate::datatypes::sqldatatypes::TdsDataType;
        use crate::datatypes::sqldatatypes::TypeInfoVariant;

        // Map TDS type to SqlDbType
        let sql_type = match col.data_type {
            TdsDataType::Int8 => SqlDbType::BigInt,
            TdsDataType::Int4 => SqlDbType::Int,
            TdsDataType::IntN => {
                // INTN can represent TinyInt, SmallInt, Int, or BigInt depending on length
                // For bulk copy, we'll infer from type_info
                match &col.type_info.type_info_variant {
                    TypeInfoVariant::VarLen(_var_type, len) => match *len {
                        1 => SqlDbType::TinyInt,
                        2 => SqlDbType::SmallInt,
                        4 => SqlDbType::Int,
                        8 => SqlDbType::BigInt,
                        _ => SqlDbType::Int,
                    },
                    _ => SqlDbType::Int,
                }
            }
            TdsDataType::Int2 => SqlDbType::SmallInt,
            TdsDataType::Int1 => SqlDbType::TinyInt,
            TdsDataType::Bit => SqlDbType::Bit,
            TdsDataType::BitN => SqlDbType::Bit,
            TdsDataType::Flt8 => SqlDbType::Float,
            TdsDataType::Flt4 => SqlDbType::Real,
            TdsDataType::FltN => {
                // FLTN can be Float or Real
                match &col.type_info.type_info_variant {
                    TypeInfoVariant::VarLen(_var_type, len) => match *len {
                        4 => SqlDbType::Real,
                        8 => SqlDbType::Float,
                        _ => SqlDbType::Float,
                    },
                    _ => SqlDbType::Float,
                }
            }
            TdsDataType::Money => SqlDbType::Money,
            TdsDataType::Money4 => SqlDbType::SmallMoney,
            TdsDataType::MoneyN => match &col.type_info.type_info_variant {
                TypeInfoVariant::VarLen(_var_type, len) => match *len {
                    4 => SqlDbType::SmallMoney,
                    8 => SqlDbType::Money,
                    _ => SqlDbType::Money,
                },
                _ => SqlDbType::Money,
            },
            TdsDataType::DateTime => SqlDbType::DateTime,
            TdsDataType::DateTim4 => SqlDbType::SmallDateTime,
            TdsDataType::DateTimeN => match &col.type_info.type_info_variant {
                TypeInfoVariant::VarLen(_var_type, len) => match *len {
                    4 => SqlDbType::SmallDateTime,
                    8 => SqlDbType::DateTime,
                    _ => SqlDbType::DateTime,
                },
                _ => SqlDbType::DateTime,
            },
            TdsDataType::DateN => SqlDbType::Date,
            TdsDataType::TimeN => SqlDbType::Time,
            TdsDataType::DateTime2N => SqlDbType::DateTime2,
            TdsDataType::DateTimeOffsetN => SqlDbType::DateTimeOffset,
            TdsDataType::BigChar => SqlDbType::Char,
            TdsDataType::BigVarChar => SqlDbType::VarChar,
            TdsDataType::VarChar => SqlDbType::VarChar,
            TdsDataType::Text => SqlDbType::Text,
            TdsDataType::NChar => SqlDbType::NChar,
            TdsDataType::NVarChar => SqlDbType::NVarChar,
            TdsDataType::NText => SqlDbType::NText,
            TdsDataType::Binary => SqlDbType::Binary,
            TdsDataType::BigBinary => SqlDbType::Binary,
            TdsDataType::BigVarBinary => SqlDbType::VarBinary,
            TdsDataType::VarBinary => SqlDbType::VarBinary,
            TdsDataType::Image => SqlDbType::Image,
            TdsDataType::Guid => SqlDbType::UniqueIdentifier,
            TdsDataType::DecimalN | TdsDataType::Decimal => SqlDbType::Decimal,
            TdsDataType::NumericN | TdsDataType::Numeric => SqlDbType::Numeric,
            TdsDataType::Xml => SqlDbType::Xml,
            TdsDataType::Json => SqlDbType::Json,
            TdsDataType::Udt => SqlDbType::Udt,
            TdsDataType::SsVariant => SqlDbType::Variant,
            TdsDataType::Vector => SqlDbType::Vector,
            _ => SqlDbType::VarChar, // Default fallback
        };

        // Extract length, precision, scale from TypeInfo
        let (length, type_length, precision, scale) = match &col.type_info.type_info_variant {
            TypeInfoVariant::FixedLen(len) => {
                let actual_len = len.get_len() as i32;
                (actual_len, TypeLength::Fixed(actual_len), 0, 0)
            }
            TypeInfoVariant::VarLen(_var_type, len) => {
                (*len as i32, TypeLength::Variable(*len as i32), 0, 0)
            }
            TypeInfoVariant::VarLenString(_len, max_len, _collation) => {
                // Check for MAX types: 0xFFFF (65535) indicates unlimited size
                if *max_len == 65535 {
                    (-1, TypeLength::Plp, 0, 0)
                } else {
                    // Use max_len for both length and type_length (column definition size)
                    (*max_len as i32, TypeLength::Variable(*max_len as i32), 0, 0)
                }
            }
            TypeInfoVariant::VarLenScale(_vlt, scale) => {
                // Use the actual length from TypeInfo, not the VariableLengthTypes enum value
                let len = col.type_info.length as i32;
                (len, TypeLength::Variable(len), 0, *scale)
            }
            TypeInfoVariant::VarLenPrecisionScale(len, _max_len, precision, scale) => (
                *len as i32,
                TypeLength::Variable(*len as i32),
                *precision,
                *scale,
            ),
            TypeInfoVariant::PartialLen(
                _plp_type,
                unknown_len,
                _collation,
                _chunk_size,
                _plp_null,
            ) => {
                // PLP types (BLOB/CLOB types)
                // For MAX types (VARCHAR(MAX), NVARCHAR(MAX), VARBINARY(MAX), etc.),
                // SQL Server sends 0xFFFF (65535) which should be treated as -1 (unlimited)
                if unknown_len.is_none() || *unknown_len == Some(65535) {
                    (-1, TypeLength::Plp, 0, 0)
                } else {
                    (unknown_len.unwrap() as i32, TypeLength::Plp, 0, 0)
                }
            }
        };

        // Get the correct TDS type for bulk copy (may differ from server's type)
        // For example, JSON (0xF4) & XML (0xF1) must be sent as NVarChar(MAX) (0xE7)
        // for bulk copy
        let tds_type = sql_type.to_bulk_copy_tds_type();

        let mut metadata = BulkCopyColumnMetadata::new(&col.column_name, sql_type, tds_type)
            .with_length(length, type_length)
            .with_nullable(col.is_nullable());

        if precision > 0 || scale > 0 {
            metadata = metadata.with_precision_scale(precision, scale);
        }

        if let Some(collation) = col.get_collation() {
            metadata = metadata.with_collation(collation);
        } else if sql_type == SqlDbType::Json {
            // JSON columns don't have collation in server metadata, but we need it
            // when sending as VARCHAR(MAX) (0xA7) for bulk copy workaround.
            // Use all-zero collation like .NET SqlBulkCopy does for JSON columns.

            metadata = metadata.with_collation(crate::token::tokens::SqlCollation {
                info: 0x00000000, // All zeros for JSON (UTF-8 encoding)
                lcid_language_id: 0,
                col_flags: 0,
                sort_id: 0,
            });
        }

        if col.is_identity() {
            metadata = metadata.with_identity(true);
        }

        metadata
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encoding_utf8() {
        let enc = EncodingType::Utf8;
        let bytes = enc.encode("Hello");
        assert_eq!(bytes, b"Hello");
        assert_eq!(enc.byte_length("Hello"), 5);
    }

    #[test]
    fn test_encoding_utf16le() {
        let enc = EncodingType::Utf16Le;
        let bytes = enc.encode("Hi");
        assert_eq!(bytes, vec![b'H', 0, b'i', 0]);
        assert_eq!(enc.byte_length("Hi"), 4);
    }

    #[test]
    fn test_encoding_latin1() {
        let enc = EncodingType::Latin1;
        let bytes = enc.encode("Café");
        // é is code point 233, which fits in Latin-1
        assert_eq!(bytes.len(), 4);
    }

    #[test]
    fn test_type_length_plp() {
        let tl = TypeLength::Plp;
        assert!(tl.is_plp());
        assert!(!tl.is_fixed());
        assert_eq!(tl.max_length(), None);
    }

    #[test]
    fn test_type_length_fixed() {
        let tl = TypeLength::Fixed(4);
        assert!(!tl.is_plp());
        assert!(tl.is_fixed());
        assert_eq!(tl.max_length(), Some(4));
    }

    #[test]
    fn test_type_length_variable() {
        let tl = TypeLength::Variable(100);
        assert!(!tl.is_plp());
        assert!(!tl.is_fixed());
        assert_eq!(tl.max_length(), Some(100));
    }

    #[test]
    fn test_metadata_builder() {
        let meta = BulkCopyColumnMetadata::new("test_col", SqlDbType::VarChar, 0xA7)
            .with_length(100, TypeLength::Variable(100))
            .with_nullable(false);

        assert_eq!(meta.column_name, "test_col");
        assert_eq!(meta.sql_type, SqlDbType::VarChar);
        assert_eq!(meta.length, 100);
        assert!(!meta.is_nullable);
    }

    #[test]
    fn test_metadata_needs_collation() {
        let meta = BulkCopyColumnMetadata::new("str_col", SqlDbType::NVarChar, 0xE7);
        assert!(meta.needs_collation());

        let meta2 = BulkCopyColumnMetadata::new("int_col", SqlDbType::Int, 0x38);
        assert!(!meta2.needs_collation());
    }

    #[test]
    fn test_metadata_needs_precision_scale() {
        let meta = BulkCopyColumnMetadata::new("dec_col", SqlDbType::Decimal, 0x6A)
            .with_precision_scale(18, 2);

        assert!(meta.needs_precision_scale());
        assert_eq!(meta.precision, 18);
        assert_eq!(meta.scale, 2);
    }

    #[test]
    fn test_metadata_is_plp() {
        let meta = BulkCopyColumnMetadata::new("max_col", SqlDbType::VarChar, 0xA7)
            .with_length(-1, TypeLength::Plp);

        assert!(meta.is_plp());
    }

    #[test]
    fn test_metadata_is_long() {
        let meta = BulkCopyColumnMetadata::new("text_col", SqlDbType::Text, 0x23);
        assert!(meta.is_long());

        let meta2 = BulkCopyColumnMetadata::new("varchar_col", SqlDbType::VarChar, 0xA7);
        assert!(!meta2.is_long());
    }

    #[test]
    fn system_type_id_conversion_all_types() {
        assert_eq!(
            SqlDbType::try_from(SystemTypeId(48)).unwrap(),
            SqlDbType::TinyInt
        );
        assert_eq!(
            SqlDbType::try_from(SystemTypeId(52)).unwrap(),
            SqlDbType::SmallInt
        );
        assert_eq!(
            SqlDbType::try_from(SystemTypeId(56)).unwrap(),
            SqlDbType::Int
        );
        assert_eq!(
            SqlDbType::try_from(SystemTypeId(127)).unwrap(),
            SqlDbType::BigInt
        );
        assert_eq!(
            SqlDbType::try_from(SystemTypeId(106)).unwrap(),
            SqlDbType::Decimal
        );
        assert_eq!(
            SqlDbType::try_from(SystemTypeId(108)).unwrap(),
            SqlDbType::Numeric
        );
        assert_eq!(
            SqlDbType::try_from(SystemTypeId(122)).unwrap(),
            SqlDbType::SmallMoney
        );
        assert_eq!(
            SqlDbType::try_from(SystemTypeId(60)).unwrap(),
            SqlDbType::Money
        );
        assert_eq!(
            SqlDbType::try_from(SystemTypeId(104)).unwrap(),
            SqlDbType::Bit
        );
        assert_eq!(
            SqlDbType::try_from(SystemTypeId(59)).unwrap(),
            SqlDbType::Real
        );
        assert_eq!(
            SqlDbType::try_from(SystemTypeId(62)).unwrap(),
            SqlDbType::Float
        );
        assert_eq!(
            SqlDbType::try_from(SystemTypeId(40)).unwrap(),
            SqlDbType::Date
        );
        assert_eq!(
            SqlDbType::try_from(SystemTypeId(41)).unwrap(),
            SqlDbType::Time
        );
        assert_eq!(
            SqlDbType::try_from(SystemTypeId(42)).unwrap(),
            SqlDbType::DateTime2
        );
        assert_eq!(
            SqlDbType::try_from(SystemTypeId(43)).unwrap(),
            SqlDbType::DateTimeOffset
        );
        assert_eq!(
            SqlDbType::try_from(SystemTypeId(58)).unwrap(),
            SqlDbType::SmallDateTime
        );
        assert_eq!(
            SqlDbType::try_from(SystemTypeId(61)).unwrap(),
            SqlDbType::DateTime
        );
        assert_eq!(
            SqlDbType::try_from(SystemTypeId(167)).unwrap(),
            SqlDbType::VarChar
        );
        assert_eq!(
            SqlDbType::try_from(SystemTypeId(175)).unwrap(),
            SqlDbType::Char
        );
        assert_eq!(
            SqlDbType::try_from(SystemTypeId(35)).unwrap(),
            SqlDbType::Text
        );
        assert_eq!(
            SqlDbType::try_from(SystemTypeId(231)).unwrap(),
            SqlDbType::NVarChar
        );
        assert_eq!(
            SqlDbType::try_from(SystemTypeId(239)).unwrap(),
            SqlDbType::NChar
        );
        assert_eq!(
            SqlDbType::try_from(SystemTypeId(99)).unwrap(),
            SqlDbType::NText
        );
        assert_eq!(
            SqlDbType::try_from(SystemTypeId(165)).unwrap(),
            SqlDbType::VarBinary
        );
        assert_eq!(
            SqlDbType::try_from(SystemTypeId(173)).unwrap(),
            SqlDbType::Binary
        );
        assert_eq!(
            SqlDbType::try_from(SystemTypeId(34)).unwrap(),
            SqlDbType::Image
        );
        assert_eq!(
            SqlDbType::try_from(SystemTypeId(36)).unwrap(),
            SqlDbType::UniqueIdentifier
        );
        assert_eq!(
            SqlDbType::try_from(SystemTypeId(241)).unwrap(),
            SqlDbType::Xml
        );
    }

    #[test]
    fn system_type_id_unsupported() {
        assert!(SqlDbType::try_from(SystemTypeId(0)).is_err());
        assert!(SqlDbType::try_from(SystemTypeId(255)).is_err());
    }

    #[test]
    fn to_tds_type_all_variants() {
        assert_eq!(SqlDbType::TinyInt.to_tds_type(), 0x26);
        assert_eq!(SqlDbType::SmallInt.to_tds_type(), 0x26);
        assert_eq!(SqlDbType::Int.to_tds_type(), 0x26);
        assert_eq!(SqlDbType::BigInt.to_tds_type(), 0x26);
        assert_eq!(SqlDbType::Bit.to_tds_type(), 0x68);
        assert_eq!(SqlDbType::Real.to_tds_type(), 0x6D);
        assert_eq!(SqlDbType::Float.to_tds_type(), 0x6D);
        assert_eq!(SqlDbType::Decimal.to_tds_type(), 0x6A);
        assert_eq!(SqlDbType::Numeric.to_tds_type(), 0x6C);
        assert_eq!(SqlDbType::Money.to_tds_type(), 0x6E);
        assert_eq!(SqlDbType::SmallMoney.to_tds_type(), 0x6E);
        assert_eq!(SqlDbType::Date.to_tds_type(), 0x28);
        assert_eq!(SqlDbType::Time.to_tds_type(), 0x29);
        assert_eq!(SqlDbType::DateTime.to_tds_type(), 0x6F);
        assert_eq!(SqlDbType::DateTime2.to_tds_type(), 0x2A);
        assert_eq!(SqlDbType::DateTimeOffset.to_tds_type(), 0x2B);
        assert_eq!(SqlDbType::SmallDateTime.to_tds_type(), 0x6F);
        assert_eq!(SqlDbType::Char.to_tds_type(), 0xAF);
        assert_eq!(SqlDbType::VarChar.to_tds_type(), 0xA7);
        assert_eq!(SqlDbType::Text.to_tds_type(), 0x23);
        assert_eq!(SqlDbType::NChar.to_tds_type(), 0xEF);
        assert_eq!(SqlDbType::NVarChar.to_tds_type(), 0xE7);
        assert_eq!(SqlDbType::NText.to_tds_type(), 0x63);
        assert_eq!(SqlDbType::Binary.to_tds_type(), 0xAD);
        assert_eq!(SqlDbType::VarBinary.to_tds_type(), 0xA5);
        assert_eq!(SqlDbType::Image.to_tds_type(), 0x22);
        assert_eq!(SqlDbType::UniqueIdentifier.to_tds_type(), 0x24);
        assert_eq!(SqlDbType::Xml.to_tds_type(), 0xF1);
        assert_eq!(SqlDbType::Json.to_tds_type(), 0xF4);
        assert_eq!(SqlDbType::Variant.to_tds_type(), 0x62);
        assert_eq!(SqlDbType::Udt.to_tds_type(), 0xF0);
        assert_eq!(SqlDbType::Vector.to_tds_type(), 0xF5);
    }

    #[test]
    fn to_bulk_copy_tds_type_xml_json_override() {
        assert_eq!(SqlDbType::Xml.to_bulk_copy_tds_type(), 0xE7);
        assert_eq!(SqlDbType::Json.to_bulk_copy_tds_type(), 0xE7);
        assert_eq!(
            SqlDbType::Int.to_bulk_copy_tds_type(),
            SqlDbType::Int.to_tds_type()
        );
    }

    #[test]
    fn builder_with_collation_name() {
        let meta = BulkCopyColumnMetadata::new("c", SqlDbType::VarChar, 0xA7)
            .with_collation_name("SQL_Latin1_General_CP1_CI_AS");
        assert_eq!(meta.collation_name.unwrap(), "SQL_Latin1_General_CP1_CI_AS");
    }

    #[test]
    fn builder_with_encoding() {
        let meta = BulkCopyColumnMetadata::new("c", SqlDbType::VarChar, 0xA7)
            .with_encoding(EncodingType::Utf8);
        assert!(meta.encoding.is_some());
    }

    #[test]
    fn builder_with_identity() {
        let meta = BulkCopyColumnMetadata::new("c", SqlDbType::Int, 0x26).with_identity(true);
        assert!(meta.is_identity);
    }

    #[test]
    fn builder_with_encrypted() {
        let meta = BulkCopyColumnMetadata::new("c", SqlDbType::Int, 0x26).with_encrypted(true);
        assert!(meta.is_encrypted);
    }

    #[test]
    fn builder_with_table_name() {
        let meta =
            BulkCopyColumnMetadata::new("c", SqlDbType::Text, 0x23).with_table_name("dbo.my_table");
        assert_eq!(meta.table_name.unwrap(), "dbo.my_table");
    }

    #[test]
    fn builder_with_scale() {
        let meta = BulkCopyColumnMetadata::new("t", SqlDbType::Time, 0x29).with_scale(7);
        assert_eq!(meta.scale, 7);
    }

    #[test]
    fn get_sql_type_definition_basic_types() {
        assert_eq!(
            BulkCopyColumnMetadata::new("c", SqlDbType::Int, 0x26)
                .get_sql_type_definition()
                .unwrap(),
            "int"
        );
        assert_eq!(
            BulkCopyColumnMetadata::new("c", SqlDbType::BigInt, 0x26)
                .get_sql_type_definition()
                .unwrap(),
            "bigint"
        );
        assert_eq!(
            BulkCopyColumnMetadata::new("c", SqlDbType::SmallInt, 0x26)
                .get_sql_type_definition()
                .unwrap(),
            "smallint"
        );
        assert_eq!(
            BulkCopyColumnMetadata::new("c", SqlDbType::TinyInt, 0x26)
                .get_sql_type_definition()
                .unwrap(),
            "tinyint"
        );
        assert_eq!(
            BulkCopyColumnMetadata::new("c", SqlDbType::Bit, 0x68)
                .get_sql_type_definition()
                .unwrap(),
            "bit"
        );
        assert_eq!(
            BulkCopyColumnMetadata::new("c", SqlDbType::Float, 0x6D)
                .get_sql_type_definition()
                .unwrap(),
            "float"
        );
        assert_eq!(
            BulkCopyColumnMetadata::new("c", SqlDbType::Real, 0x6D)
                .get_sql_type_definition()
                .unwrap(),
            "real"
        );
        assert_eq!(
            BulkCopyColumnMetadata::new("c", SqlDbType::Money, 0x6E)
                .get_sql_type_definition()
                .unwrap(),
            "money"
        );
        assert_eq!(
            BulkCopyColumnMetadata::new("c", SqlDbType::SmallMoney, 0x6E)
                .get_sql_type_definition()
                .unwrap(),
            "smallmoney"
        );
        assert_eq!(
            BulkCopyColumnMetadata::new("c", SqlDbType::UniqueIdentifier, 0x24)
                .get_sql_type_definition()
                .unwrap(),
            "uniqueidentifier"
        );
        assert_eq!(
            BulkCopyColumnMetadata::new("c", SqlDbType::DateTime, 0x6F)
                .get_sql_type_definition()
                .unwrap(),
            "datetime"
        );
        assert_eq!(
            BulkCopyColumnMetadata::new("c", SqlDbType::SmallDateTime, 0x6F)
                .get_sql_type_definition()
                .unwrap(),
            "smalldatetime"
        );
        assert_eq!(
            BulkCopyColumnMetadata::new("c", SqlDbType::Date, 0x28)
                .get_sql_type_definition()
                .unwrap(),
            "date"
        );
        assert_eq!(
            BulkCopyColumnMetadata::new("c", SqlDbType::Text, 0x23)
                .get_sql_type_definition()
                .unwrap(),
            "text"
        );
        assert_eq!(
            BulkCopyColumnMetadata::new("c", SqlDbType::NText, 0x63)
                .get_sql_type_definition()
                .unwrap(),
            "ntext"
        );
        assert_eq!(
            BulkCopyColumnMetadata::new("c", SqlDbType::Image, 0x22)
                .get_sql_type_definition()
                .unwrap(),
            "image"
        );
        assert_eq!(
            BulkCopyColumnMetadata::new("c", SqlDbType::Xml, 0xF1)
                .get_sql_type_definition()
                .unwrap(),
            "xml"
        );
        assert_eq!(
            BulkCopyColumnMetadata::new("c", SqlDbType::Variant, 0x62)
                .get_sql_type_definition()
                .unwrap(),
            "sql_variant"
        );
        assert_eq!(
            BulkCopyColumnMetadata::new("c", SqlDbType::Json, 0xF4)
                .get_sql_type_definition()
                .unwrap(),
            "nvarchar(max)"
        );
    }

    #[test]
    fn get_sql_type_definition_parameterized() {
        let meta =
            BulkCopyColumnMetadata::new("c", SqlDbType::Decimal, 0x6A).with_precision_scale(18, 4);
        assert_eq!(meta.get_sql_type_definition().unwrap(), "decimal(18, 4)");

        let meta = BulkCopyColumnMetadata::new("c", SqlDbType::Time, 0x29).with_scale(3);
        assert_eq!(meta.get_sql_type_definition().unwrap(), "time(3)");

        let meta = BulkCopyColumnMetadata::new("c", SqlDbType::DateTime2, 0x2A).with_scale(7);
        assert_eq!(meta.get_sql_type_definition().unwrap(), "datetime2(7)");

        let meta = BulkCopyColumnMetadata::new("c", SqlDbType::DateTimeOffset, 0x2B).with_scale(0);
        assert_eq!(meta.get_sql_type_definition().unwrap(), "datetimeoffset(0)");
    }

    #[test]
    fn get_sql_type_definition_string_types() {
        // NVARCHAR with byte-length 200 → char count 100
        let meta = BulkCopyColumnMetadata::new("c", SqlDbType::NVarChar, 0xE7)
            .with_length(200, TypeLength::Variable(200));
        assert_eq!(meta.get_sql_type_definition().unwrap(), "nvarchar(100)");

        // NVARCHAR(MAX)
        let meta = BulkCopyColumnMetadata::new("c", SqlDbType::NVarChar, 0xE7)
            .with_length(-1, TypeLength::Plp);
        assert_eq!(meta.get_sql_type_definition().unwrap(), "nvarchar(max)");

        // NCHAR with byte-length 40 → char count 20
        let meta = BulkCopyColumnMetadata::new("c", SqlDbType::NChar, 0xEF)
            .with_length(40, TypeLength::Variable(40));
        assert_eq!(meta.get_sql_type_definition().unwrap(), "nchar(20)");

        // VARCHAR
        let meta = BulkCopyColumnMetadata::new("c", SqlDbType::VarChar, 0xA7)
            .with_length(50, TypeLength::Variable(50));
        assert_eq!(meta.get_sql_type_definition().unwrap(), "varchar(50)");

        // VARCHAR(MAX)
        let meta = BulkCopyColumnMetadata::new("c", SqlDbType::VarChar, 0xA7)
            .with_length(-1, TypeLength::Plp);
        assert_eq!(meta.get_sql_type_definition().unwrap(), "varchar(max)");

        // CHAR
        let meta = BulkCopyColumnMetadata::new("c", SqlDbType::Char, 0xAF)
            .with_length(10, TypeLength::Fixed(10));
        assert_eq!(meta.get_sql_type_definition().unwrap(), "char(10)");

        // VARBINARY
        let meta = BulkCopyColumnMetadata::new("c", SqlDbType::VarBinary, 0xA5)
            .with_length(100, TypeLength::Variable(100));
        assert_eq!(meta.get_sql_type_definition().unwrap(), "varbinary(100)");

        // VARBINARY(MAX)
        let meta = BulkCopyColumnMetadata::new("c", SqlDbType::VarBinary, 0xA5)
            .with_length(-1, TypeLength::Plp);
        assert_eq!(meta.get_sql_type_definition().unwrap(), "varbinary(max)");

        // BINARY
        let meta = BulkCopyColumnMetadata::new("c", SqlDbType::Binary, 0xAD)
            .with_length(16, TypeLength::Fixed(16));
        assert_eq!(meta.get_sql_type_definition().unwrap(), "binary(16)");

        // UDT
        let meta = BulkCopyColumnMetadata::new("c", SqlDbType::Udt, 0xF0)
            .with_length(256, TypeLength::Variable(256));
        assert_eq!(meta.get_sql_type_definition().unwrap(), "varbinary(256)");
    }

    #[test]
    fn vector_dimensions_valid() {
        use crate::datatypes::sqldatatypes::VECTOR_HEADER_SIZE;
        // Float32 (scale=0), element_size=4, 3 dimensions: header(8) + 3*4 = 20
        let meta = BulkCopyColumnMetadata::new("v", SqlDbType::Vector, 0xF5)
            .with_length(
                (VECTOR_HEADER_SIZE + 3 * 4) as i32,
                TypeLength::Variable((VECTOR_HEADER_SIZE + 3 * 4) as i32),
            )
            .with_scale(0);
        assert_eq!(meta.vector_dimensions().unwrap(), 3);
    }

    #[test]
    fn vector_dimensions_not_vector_type() {
        let meta = BulkCopyColumnMetadata::new("c", SqlDbType::Int, 0x26);
        assert!(meta.vector_dimensions().is_err());
    }

    #[test]
    fn vector_dimensions_too_small_length() {
        let meta = BulkCopyColumnMetadata::new("v", SqlDbType::Vector, 0xF5)
            .with_length(2, TypeLength::Variable(2))
            .with_scale(0);
        assert!(meta.vector_dimensions().is_err());
    }

    #[test]
    fn vector_dimensions_not_divisible() {
        use crate::datatypes::sqldatatypes::VECTOR_HEADER_SIZE;
        // Float32 (scale=0), element_size=4, payload=5 (not divisible by 4)
        let meta = BulkCopyColumnMetadata::new("v", SqlDbType::Vector, 0xF5)
            .with_length(
                (VECTOR_HEADER_SIZE + 5) as i32,
                TypeLength::Variable((VECTOR_HEADER_SIZE + 5) as i32),
            )
            .with_scale(0);
        assert!(meta.vector_dimensions().is_err());
    }

    #[test]
    fn vector_sql_type_definition() {
        use crate::datatypes::sqldatatypes::VECTOR_HEADER_SIZE;
        let meta = BulkCopyColumnMetadata::new("v", SqlDbType::Vector, 0xF5)
            .with_length(
                (VECTOR_HEADER_SIZE + 3 * 4) as i32,
                TypeLength::Variable((VECTOR_HEADER_SIZE + 3 * 4) as i32),
            )
            .with_scale(0);
        assert_eq!(meta.get_sql_type_definition().unwrap(), "vector(3)");
    }

    #[test]
    fn encoding_type_latin1_non_latin() {
        let enc = EncodingType::Latin1;
        let bytes = enc.encode("日本語");
        assert!(bytes.iter().all(|&b| b == b'?'));
    }

    #[test]
    fn encoding_type_codepage() {
        let enc = EncodingType::CodePage(1252);
        let bytes = enc.encode("Hello");
        assert_eq!(bytes, b"Hello");
        assert_eq!(enc.byte_length("Hello"), 5);
    }

    #[test]
    fn encoding_type_unicode() {
        assert!(matches!(EncodingType::unicode(), EncodingType::Utf16Le));
    }

    #[test]
    fn encoding_type_default_ansi() {
        assert!(matches!(EncodingType::default_ansi(), EncodingType::Utf8));
    }

    #[test]
    fn default_metadata() {
        let meta = BulkCopyColumnMetadata::default();
        assert_eq!(meta.sql_type, SqlDbType::Int);
        assert_eq!(meta.tds_type, 0x38);
        assert!(meta.is_nullable);
        assert!(!meta.is_identity);
    }
}

// Include additional unit tests from separate test file
#[cfg(test)]
#[path = "bulk_copy_metadata_tests.rs"]
mod bulk_copy_metadata_tests;
