// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Metadata retrieval for bulk copy operations.
//!
//! This module provides traits and implementations for retrieving table metadata
//! from SQL Server. The metadata is used to automatically map columns and validate
//! types during bulk copy operations.
//!
//! # Implementations
//!
//! - [`FmtOnlyMetadataRetriever`]: Uses `SET FMTONLY ON` for fast metadata retrieval
//!
//! # Custom Retrievers
//!
//! You can implement the [`MetadataRetriever`] trait to provide custom metadata
//! retrieval strategies, such as caching or alternative sources.

use crate::connection::tds_client::{ResultSetClient, TdsClient};
use crate::core::{CancelHandle, TdsResult};
use crate::datatypes::bulk_copy_metadata::BulkCopyColumnMetadata;
use crate::error::Error;
use crate::sql_identifier::{
    CATALOG_INDEX, SCHEMA_INDEX, TABLE_INDEX, build_multipart_name, escape_identifier,
    escape_string_literal, parse_multipart_identifier,
};
use crate::token::tokens::ColMetadataToken;
use async_trait::async_trait;
use tracing::{debug, instrument, trace};

/// Result of fetching table metadata, including collation information.
#[derive(Debug, Clone)]
pub(crate) struct TableMetadataResult {
    /// Column metadata from the COLMETADATA token
    pub col_metadata: ColMetadataToken,
    /// Collation names for each column (index matches column order)
    /// Only populated for character type columns
    pub collation_names: Vec<Option<String>>,
}

/// Trait for retrieving destination table metadata.
///
/// This trait allows different strategies for retrieving table metadata,
/// such as querying system catalogs, caching, or using alternative sources.
///
/// # Example
///
/// ```rust,ignore
/// use mssql_tds::connection::metadata_retriever::{MetadataRetriever};
/// use mssql_tds::datatypes::bulk_copy_metadata::BulkCopyColumnMetadata;
///
/// struct CachedMetadataRetriever {
///     cache: HashMap<String, Vec<BulkCopyColumnMetadata>>,
/// }
///
/// #[async_trait]
/// impl MetadataRetriever for CachedMetadataRetriever {
///     async fn retrieve_metadata(
///         &mut self,
///         client: &mut TdsClient,
///         table_name: &str,
///         timeout_sec: u32,
///     ) -> TdsResult<Vec<BulkCopyColumnMetadata>> {
///         if let Some(metadata) = self.cache.get(table_name) {
///             return Ok(metadata.clone());
///         }
///         // Fallback to default retrieval...
///     }
/// }
/// ```
#[async_trait]
pub trait MetadataRetriever: Send {
    /// Retrieve metadata for the specified table.
    ///
    /// # Arguments
    ///
    /// * `client` - Mutable reference to the TDS client for executing queries
    /// * `table_name` - Name of the destination table (may include schema)
    /// * `timeout_sec` - Query timeout in seconds
    ///
    /// # Returns
    ///
    /// A vector of `BulkCopyColumnMetadata` containing column information
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Table does not exist
    /// - No permission to access metadata
    /// - Network errors during query execution
    async fn retrieve_metadata(
        &mut self,
        client: &mut TdsClient,
        table_name: &str,
        timeout_sec: u32,
    ) -> TdsResult<Vec<BulkCopyColumnMetadata>>;
}

/// Metadata retriever using SET FMTONLY ON query.
///
/// This implementation retrieves table metadata by executing `SET FMTONLY ON; SELECT * FROM table`
/// which returns only the COLMETADATA token without any rows. This is faster than
/// querying system catalog views and provides complete metadata including:
/// - Column names, types, nullability
/// - Identity and computed column flags (from TDS COLMETADATA flags)
/// - Precision, scale, and collation information
///
/// # Advantages
///
/// - Fast: Single round-trip with SET FMTONLY
/// - Works with any user permissions that allow SELECT on the table
/// - Handles temporal tables and SQL Graph columns correctly (via dynamic column list)
///
/// # When to use
///
/// Use this retriever when:
/// - You need fast metadata retrieval
/// - You have SELECT permission but not sys.columns access
/// - Performance is critical for bulk operations
///
/// # Example
///
/// ```rust,ignore
/// let retriever = FmtOnlyMetadataRetriever::new();
/// let bulk_copy = BulkCopy::with_retriever(
///     &mut client,
///     "MyTable",
///     Box::new(retriever)
/// );
/// ```
#[derive(Debug, Default)]
pub struct FmtOnlyMetadataRetriever;

impl FmtOnlyMetadataRetriever {
    /// Create a new FMTONLY metadata retriever.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl MetadataRetriever for FmtOnlyMetadataRetriever {
    async fn retrieve_metadata(
        &mut self,
        client: &mut TdsClient,
        table_name: &str,
        timeout_sec: u32,
    ) -> TdsResult<Vec<BulkCopyColumnMetadata>> {
        // Convert 0 → None (infinite timeout) to match TdsClient convention
        let timeout = if timeout_sec == 0 {
            None
        } else {
            Some(timeout_sec)
        };
        // Fetch metadata using SET FMTONLY ON and sp_tablecollations_100
        let metadata_result = fetch_table_metadata(client, table_name, timeout, None).await?;

        // Convert using TryFrom trait - directly to BulkCopyColumnMetadata
        Vec::<BulkCopyColumnMetadata>::try_from(metadata_result)
    }
}

impl TryFrom<TableMetadataResult> for Vec<BulkCopyColumnMetadata> {
    type Error = Error;

    fn try_from(result: TableMetadataResult) -> Result<Self, Self::Error> {
        if result.col_metadata.columns.is_empty() {
            return Err(Error::UsageError(
                "Table not found or has no columns".to_string(),
            ));
        }

        // Convert each column using the existing From<&ColumnMetadata> implementation
        let mut metadata: Vec<BulkCopyColumnMetadata> = result
            .col_metadata
            .columns
            .iter()
            .map(BulkCopyColumnMetadata::from)
            .collect();

        // Add collation names to the metadata
        for (i, col_meta) in metadata.iter_mut().enumerate() {
            if i < result.collation_names.len() {
                col_meta.collation_name = result.collation_names[i].clone();
            }
        }

        // Capture Always Encrypted material for encrypted columns. When column
        // encryption is negotiated the destination COLMETADATA carries a CEK
        // table plus per-column crypto metadata; pair each encrypted column with
        // the CEK table entry it references so the bulk-copy writer can encrypt
        // values and emit the encrypted COLMETADATA.
        #[cfg(feature = "column-encryption")]
        for (i, col_meta) in metadata.iter_mut().enumerate() {
            let Some(crypto) = result
                .col_metadata
                .columns
                .get(i)
                .and_then(|c| c.crypto_metadata.as_ref())
            else {
                continue;
            };
            let cek_entry = result
                .col_metadata
                .cek_table
                .get(crypto.cek_table_ordinal as usize)
                .ok_or_else(|| {
                    Error::ProtocolError(format!(
                        "Encrypted column '{}' references CEK ordinal {} but the CEK table has \
                         {} entries",
                        col_meta.column_name,
                        crypto.cek_table_ordinal,
                        result.col_metadata.cek_table.len()
                    ))
                })?;
            col_meta.encryption = Some(
                crate::datatypes::bulk_copy_metadata::BulkCopyColumnEncryption {
                    crypto_metadata: crypto.clone(),
                    cek_entry: cek_entry.clone(),
                },
            );
        }

        Ok(metadata)
    }
}

/// Fetch table metadata using SET FMTONLY ON query.
///
/// This function retrieves column metadata for a table by executing a
/// query with `SET FMTONLY ON`, which returns only the COLMETADATA
/// token without any row data. It also fetches collation information
/// by calling sp_tablecollations_100.
///
/// # Arguments
///
/// * `client` - Mutable reference to the TDS client
/// * `table_name` - Name of the table, supporting multipart names:
///   - 1-part: "table" (uses current database and default schema)
///   - 2-part: "schema.table"
///   - 3-part: "database.schema.table"
///   - 4-part: "server.database.schema.table" (server part ignored in query context)
/// * `timeout_sec` - Query timeout in seconds
/// * `cancel_handle` - Optional cancellation handle
///
/// # Returns
///
/// A `TableMetadataResult` containing column metadata and collation names.
///
/// # Errors
///
/// Returns an error if:
/// - Table does not exist
/// - No permission to access the table
/// - Network errors during query execution
/// - Invalid multipart identifier format
///
/// # Implementation Notes
///
/// This function is designed for bulk copy operations where we need table schema
/// without the overhead of querying system catalog views. It dynamically builds the column
/// list to:
/// - Support hidden columns in temporal tables
/// - Exclude SQL Graph columns that cannot be selected
/// - Handle multipart table names with proper catalog prefix
/// - Retrieve collation names for character columns via sp_tablecollations_100
#[instrument(skip(client), level = "info")]
pub(crate) async fn fetch_table_metadata(
    client: &mut TdsClient,
    table_name: &str,
    timeout_sec: Option<u32>,
    cancel_handle: Option<&CancelHandle>,
) -> TdsResult<TableMetadataResult> {
    // Parse the multipart identifier
    let parts = parse_multipart_identifier(table_name, false)?;

    // Validate table name exists
    let table_part = parts[TABLE_INDEX]
        .as_ref()
        .ok_or_else(|| Error::UsageError(format!("Invalid table name: {}", table_name)))?;

    // Check if temp table
    let is_temp_table = table_part.starts_with('#');

    // Determine catalog
    let catalog = if is_temp_table && parts[CATALOG_INDEX].is_none() {
        "tempdb".to_string()
    } else if let Some(cat) = &parts[CATALOG_INDEX] {
        escape_identifier(cat)
    } else {
        // No catalog specified, don't prefix
        String::new()
    };

    // Build full object name for OBJECT_ID
    let full_name = build_multipart_name(&parts);
    let escaped_full_name = escape_string_literal(&full_name);

    // Build query with catalog prefix for sys views (single dot)
    // and catalog spec for stored procedures (double dot)
    let catalog_prefix = if !catalog.is_empty() {
        format!("{}.", catalog)
    } else {
        String::new()
    };

    let catalog_for_sproc = if !catalog.is_empty() {
        format!("{}..", catalog)
    } else {
        String::new()
    };

    // Prepare schema and table names for sp_tablecollations_100
    // Match C# behavior: escape for use in TSQL literal block
    let schema_name = parts[SCHEMA_INDEX]
        .as_ref()
        .map(|s| escape_identifier(&escape_string_literal(s)))
        .unwrap_or_else(|| "dbo".to_string());

    let table_name_escaped = escape_identifier(&escape_string_literal(table_part));

    // Use SET FMTONLY ON to get metadata without query execution overhead.
    // This matches .NET SqlBulkCopy behavior and is more efficient than SELECT TOP 0.
    // The query structure matches C# SqlBulkCopy.CreateInitialQuery():
    // 1. SELECT @@TRANCOUNT - produces a result set we need to skip
    // 2. Dynamic column building with graph_type check
    // 3. SET FMTONLY ON to get column metadata without data
    // 4. sp_tablecollations_100 to get collation information
    // Note: Use double-dot notation (catalog..sproc) for system stored procedures.
    let query = format!(
        r#"SELECT @@TRANCOUNT;

DECLARE @Column_Names NVARCHAR(MAX) = NULL;
DECLARE @object_id INT = OBJECT_ID('{escaped_full_name}');
DECLARE @sql NVARCHAR(MAX);
SET @sql = N'SELECT @CN = COALESCE(@CN + N'', '', N'''') + QUOTENAME([name]) FROM {catalog_prefix}sys.all_columns WHERE [object_id] = @ObjId';
IF EXISTS (SELECT TOP 1 * FROM sys.all_columns WHERE [object_id] = OBJECT_ID('sys.all_columns') AND [name] = 'graph_type')
    SET @sql = @sql + N' AND COALESCE([graph_type], 0) NOT IN (1, 3, 4, 6, 7)';
SET @sql = @sql + N' ORDER BY [column_id] ASC';
EXEC sp_executesql @sql, N'@CN NVARCHAR(MAX) OUTPUT, @ObjId INT', @CN = @Column_Names OUTPUT, @ObjId = @object_id;

SELECT @Column_Names = COALESCE(@Column_Names, '*');

SET FMTONLY ON;
EXEC(N'SELECT ' + @Column_Names + N' FROM {escaped_full_name}');
SET FMTONLY OFF;

EXEC {catalog_for_sproc}sp_tablecollations_100 N'{schema_name}.{table_name_escaped}';"#
    );

    debug!("Fetching table metadata with FMTONLY and collations");

    // Execute the query
    client.execute(query, timeout_sec, cancel_handle).await?;

    // Result set 1: @@TRANCOUNT - execute() positions us at the first ColMetadata
    // Consume this result set (should have exactly 1 row)
    trace!("Consuming @@TRANCOUNT result set");
    while let Some(_row) = client.get_next_row().await? {
        // Skip rows from @@TRANCOUNT (just 1 row with value 0)
    }

    // Move to result set 2: FMTONLY metadata
    // This will navigate through multiple DONE tokens from the variable declarations
    // and SET FMTONLY statements until it finds the COLMETADATA for the EXEC() result
    trace!("Moving to FMTONLY metadata result set");
    if !client.move_to_next().await? {
        return Err(Error::UsageError(format!(
            "Failed to move to FMTONLY metadata for table {}",
            table_name
        )));
    }

    // Get the metadata from the FMTONLY result set
    let col_metadata = client
        .get_current_metadata()
        .ok_or_else(|| {
            Error::UsageError(format!("Failed to fetch metadata for table {table_name}"))
        })?
        .clone();

    debug!(
        "Fetched {} columns from table metadata",
        col_metadata.columns.len()
    );
    for (i, col) in col_metadata.columns.iter().enumerate() {
        trace!(
            "Column {}: name='{}', tds_type=0x{:02X}, nullable={}",
            i,
            col.column_name,
            col.data_type as u8,
            col.is_nullable()
        );
    }

    // The FMTONLY query doesn't return any rows, just metadata
    // Consume any rows (should be none with FMTONLY)
    trace!("Consuming FMTONLY result set rows (should be none)");
    while let Some(_row) = client.get_next_row().await? {
        // Skip any rows (FMTONLY should return 0 rows)
    }

    // Move to result set 3: collations from sp_tablecollations_100
    // This will navigate through SET FMTONLY OFF, EXEC sp_tablecollations_100,
    // ReturnStatus, and DoneProc tokens until it finds the COLMETADATA
    trace!("Moving to sp_tablecollations_100 result set");
    if !client.move_to_next().await? {
        return Err(Error::UsageError(format!(
            "Failed to move to collation metadata for table {}",
            table_name
        )));
    }

    // Read collation names from the result set
    // sp_tablecollations_100 returns columns: colid, name, tds_collation, collation
    // We need the collation (column index 3, the string collation name)
    let mut collation_names: Vec<Option<String>> = Vec::new();

    trace!("Reading collation data from sp_tablecollations_100");
    while let Some(row) = client.get_next_row().await? {
        // Column 3 (0-indexed) is the collation_name
        if row.len() > 3 {
            let collation_value = &row[3];
            let collation_name = match collation_value {
                crate::datatypes::column_values::ColumnValues::String(s) => Some(s.to_string()),
                crate::datatypes::column_values::ColumnValues::Null => None,
                _ => {
                    trace!("Unexpected collation value type: {:?}", collation_value);
                    None
                }
            };

            if let Some(name) = &collation_name {
                trace!("Column {} collation: {}", collation_names.len(), name);
            }

            collation_names.push(collation_name);
        } else {
            // If row doesn't have enough columns, push None
            trace!("Row has fewer than 4 columns, using None for collation");
            collation_names.push(None);
        }
    }

    debug!("Fetched {} collation names", collation_names.len());

    // Ensure we have the same number of collations as columns
    // If not, pad with None
    while collation_names.len() < col_metadata.columns.len() {
        collation_names.push(None);
    }

    // Close the query to free up the connection
    client.close_query().await?;

    Ok(TableMetadataResult {
        col_metadata,
        collation_names,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::datatypes::sqldatatypes::{
        FixedLengthTypes, TdsDataType, TypeInfo, TypeInfoVariant, VariableLengthTypes,
    };
    use crate::query::metadata::ColumnMetadata;
    use crate::token::tokens::SqlCollation;

    fn make_column(name: &str, flags: u16, type_info_variant: TypeInfoVariant) -> ColumnMetadata {
        let tds_type = match &type_info_variant {
            TypeInfoVariant::FixedLen(_) => TdsDataType::IntN,
            TypeInfoVariant::VarLenString(_, _, _) => TdsDataType::NVarChar,
            _ => TdsDataType::IntN,
        };
        ColumnMetadata {
            user_type: 0,
            flags,
            type_info: TypeInfo {
                tds_type,
                length: 4,
                type_info_variant,
            },
            data_type: tds_type,
            column_name: name.to_string(),
            multi_part_name: None,
            crypto_metadata: None,
        }
    }

    fn make_table_metadata(
        columns: Vec<ColumnMetadata>,
        collation_names: Vec<Option<String>>,
    ) -> TableMetadataResult {
        TableMetadataResult {
            col_metadata: ColMetadataToken {
                column_count: columns.len() as u16,
                columns,
                cek_table: Vec::new(),
            },
            collation_names,
        }
    }

    fn nvarchar_variant() -> TypeInfoVariant {
        TypeInfoVariant::VarLenString(
            VariableLengthTypes::NVarChar,
            100,
            Some(SqlCollation {
                info: 0,
                lcid_language_id: 0,
                col_flags: 0,
                sort_id: 0,
            }),
        )
    }

    #[test]
    fn test_try_from_table_metadata_result_empty_columns() {
        let result = make_table_metadata(vec![], vec![]);
        let err = Vec::<BulkCopyColumnMetadata>::try_from(result).unwrap_err();
        assert!(matches!(err, Error::UsageError(msg) if msg.contains("no columns")),);
    }

    #[test]
    fn test_try_from_table_metadata_result_with_collations() {
        let columns = vec![
            make_column("col1", 0x00, nvarchar_variant()),
            make_column("col2", 0x00, nvarchar_variant()),
        ];
        let collations = vec![
            Some("Latin1_General_CI_AS".to_string()),
            Some("SQL_Latin1_General_CP1_CI_AS".to_string()),
        ];
        let result = make_table_metadata(columns, collations);
        let metadata = Vec::<BulkCopyColumnMetadata>::try_from(result).unwrap();

        assert_eq!(metadata.len(), 2);
        assert_eq!(
            metadata[0].collation_name.as_deref(),
            Some("Latin1_General_CI_AS")
        );
        assert_eq!(
            metadata[1].collation_name.as_deref(),
            Some("SQL_Latin1_General_CP1_CI_AS")
        );
    }

    #[test]
    fn test_try_from_table_metadata_result_collations_shorter_than_columns() {
        let columns = vec![
            make_column("col1", 0x00, nvarchar_variant()),
            make_column("col2", 0x00, nvarchar_variant()),
            make_column(
                "col3",
                0x00,
                TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
            ),
        ];
        let collations = vec![Some("Latin1_General_CI_AS".to_string())];
        let result = make_table_metadata(columns, collations);
        let metadata = Vec::<BulkCopyColumnMetadata>::try_from(result).unwrap();

        assert_eq!(metadata.len(), 3);
        assert_eq!(
            metadata[0].collation_name.as_deref(),
            Some("Latin1_General_CI_AS")
        );
        assert!(metadata[1].collation_name.is_none());
        assert!(metadata[2].collation_name.is_none());
    }

    #[test]
    fn test_try_from_table_metadata_result_no_collations() {
        let columns = vec![
            make_column("col1", 0x00, nvarchar_variant()),
            make_column(
                "col2",
                0x00,
                TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
            ),
        ];
        let result = make_table_metadata(columns, vec![]);
        let metadata = Vec::<BulkCopyColumnMetadata>::try_from(result).unwrap();

        assert_eq!(metadata.len(), 2);
        assert!(metadata[0].collation_name.is_none());
        assert!(metadata[1].collation_name.is_none());
    }

    #[test]
    fn test_try_from_table_metadata_result_preserves_identity_flag() {
        let columns = vec![
            make_column(
                "id",
                0x10,
                TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
            ),
            make_column("name", 0x00, nvarchar_variant()),
        ];
        let result = make_table_metadata(columns, vec![]);
        let metadata = Vec::<BulkCopyColumnMetadata>::try_from(result).unwrap();

        assert!(metadata[0].is_identity);
        assert!(!metadata[1].is_identity);
    }

    #[cfg(feature = "column-encryption")]
    #[test]
    fn test_try_from_table_metadata_captures_encryption_material() {
        use crate::query::metadata::{CekTableEntry, CryptoMetadata};

        // Encrypted column: the COLMETADATA flags carry the encrypted bit and the
        // column carries crypto metadata referencing CEK table ordinal 0.
        let mut col = make_column(
            "secret",
            0x0800, // FLAG_ENCRYPTED
            TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
        );
        col.crypto_metadata = Some(CryptoMetadata {
            cek_table_ordinal: 0,
            base_data_type: TdsDataType::IntN,
            base_type_info: TypeInfo {
                tds_type: TdsDataType::IntN,
                length: 4,
                type_info_variant: TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
            },
            cipher_algorithm_id: 2,
            cipher_algorithm_name: None,
            encryption_type: 1,
            normalization_rule_version: 1,
        });

        let cek_entry = CekTableEntry {
            database_id: 7,
            cek_id: 11,
            cek_version: 1,
            cek_md_version: [1, 2, 3, 4, 5, 6, 7, 8],
            encrypted_cek_values: Vec::new(),
        };

        let mut table = make_table_metadata(vec![col], vec![None]);
        table.col_metadata.cek_table = vec![cek_entry];

        let metadata = Vec::<BulkCopyColumnMetadata>::try_from(table).unwrap();
        assert_eq!(metadata.len(), 1);
        assert!(metadata[0].is_encrypted);
        let enc = metadata[0]
            .encryption
            .as_ref()
            .expect("encryption material should be captured for an encrypted column");
        assert_eq!(enc.crypto_metadata.encryption_type, 1);
        assert_eq!(enc.crypto_metadata.cek_table_ordinal, 0);
        assert_eq!(enc.cek_entry.database_id, 7);
        assert_eq!(enc.cek_entry.cek_id, 11);
    }

    #[cfg(feature = "column-encryption")]
    #[test]
    fn test_try_from_table_metadata_rejects_dangling_cek_ordinal() {
        use crate::query::metadata::CryptoMetadata;

        let mut col = make_column(
            "secret",
            0x0800,
            TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
        );
        col.crypto_metadata = Some(CryptoMetadata {
            cek_table_ordinal: 3, // no such entry
            base_data_type: TdsDataType::IntN,
            base_type_info: TypeInfo {
                tds_type: TdsDataType::IntN,
                length: 4,
                type_info_variant: TypeInfoVariant::FixedLen(FixedLengthTypes::Int4),
            },
            cipher_algorithm_id: 2,
            cipher_algorithm_name: None,
            encryption_type: 1,
            normalization_rule_version: 1,
        });

        let table = make_table_metadata(vec![col], vec![None]);
        let err = Vec::<BulkCopyColumnMetadata>::try_from(table).unwrap_err();
        assert!(matches!(err, Error::ProtocolError(_)));
    }
}
