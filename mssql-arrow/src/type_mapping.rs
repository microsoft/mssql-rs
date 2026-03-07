// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use arrow_schema::DataType;
use mssql_tds::datatypes::bulk_copy_metadata::{BulkCopyColumnMetadata, EncodingType, SqlDbType};

use crate::error::ArrowError;

/// Resolved mapping between one Arrow column and one SQL Server destination column.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub(crate) struct ResolvedTypeMapping {
    pub source_index: usize,
    pub dest_index: usize,
    pub arrow_type: DataType,
    pub sql_type: SqlDbType,
    pub precision: u8,
    pub scale: u8,
    pub max_length: i32,
    pub encoding: Option<EncodingType>,
    pub is_nullable: bool,
    pub is_plp: bool,
}

/// Registry of per-column type mappings between an Arrow schema and a SQL Server destination.
#[derive(Debug)]
pub(crate) struct TypeMappingRegistry {
    pub mappings: Vec<ResolvedTypeMapping>,
}

impl TypeMappingRegistry {
    /// Resolve type mappings from an Arrow schema against destination column metadata.
    ///
    /// Validates that every Arrow column has a compatible SQL Server type mapping.
    /// Columns are matched positionally (1:1 by index).
    pub fn resolve(
        arrow_schema: &arrow_schema::Schema,
        dest_metadata: &[BulkCopyColumnMetadata],
    ) -> Result<Self, ArrowError> {
        if arrow_schema.fields().is_empty() {
            return Err(ArrowError::EmptySchema);
        }

        if arrow_schema.fields().len() != dest_metadata.len() {
            return Err(ArrowError::ColumnCountMismatch {
                arrow_columns: arrow_schema.fields().len(),
                sql_columns: dest_metadata.len(),
            });
        }

        let mut mappings = Vec::with_capacity(arrow_schema.fields().len());
        for (i, (field, dest)) in arrow_schema
            .fields()
            .iter()
            .zip(dest_metadata.iter())
            .enumerate()
        {
            validate_type_compatibility(field.name(), field.data_type(), dest)?;

            mappings.push(ResolvedTypeMapping {
                source_index: i,
                dest_index: i,
                arrow_type: field.data_type().clone(),
                sql_type: dest.sql_type,
                precision: dest.precision,
                scale: dest.scale,
                max_length: dest.length,
                encoding: dest.encoding.clone(),
                is_nullable: dest.is_nullable,
                is_plp: dest.length_type.is_plp(),
            });
        }

        Ok(Self { mappings })
    }

    /// Resolve type mappings by matching Arrow field names to SQL Server column names
    /// (case-insensitive). Unmatched nullable destination columns are skipped.
    ///
    /// Returns an error if any Arrow field has no matching destination column,
    /// or if a matched pair has incompatible types.
    #[allow(dead_code)] // Will be called via ArrowBulkCopy auto-matching
    pub fn resolve_by_name(
        arrow_schema: &arrow_schema::Schema,
        dest_metadata: &[BulkCopyColumnMetadata],
    ) -> Result<Self, ArrowError> {
        if arrow_schema.fields().is_empty() {
            return Err(ArrowError::EmptySchema);
        }

        let mut mappings = Vec::with_capacity(arrow_schema.fields().len());

        for (src_idx, field) in arrow_schema.fields().iter().enumerate() {
            let name_lower = field.name().to_lowercase();
            let dest_match = dest_metadata
                .iter()
                .enumerate()
                .find(|(_, d)| d.column_name.to_lowercase() == name_lower);

            match dest_match {
                Some((dest_idx, dest)) => {
                    validate_type_compatibility(field.name(), field.data_type(), dest)?;
                    mappings.push(ResolvedTypeMapping {
                        source_index: src_idx,
                        dest_index: dest_idx,
                        arrow_type: field.data_type().clone(),
                        sql_type: dest.sql_type,
                        precision: dest.precision,
                        scale: dest.scale,
                        max_length: dest.length,
                        encoding: dest.encoding.clone(),
                        is_nullable: dest.is_nullable,
                        is_plp: dest.length_type.is_plp(),
                    });
                }
                None => {
                    return Err(ArrowError::TypeMismatch {
                        column_name: field.name().clone(),
                        arrow_type: field.data_type().clone(),
                        sql_type: SqlDbType::Int, // placeholder; no match found
                    });
                }
            }
        }

        Ok(Self { mappings })
    }
}

/// Validate that an Arrow DataType is compatible with the destination SQL Server column.
fn validate_type_compatibility(
    column_name: &str,
    arrow_type: &DataType,
    dest: &BulkCopyColumnMetadata,
) -> Result<(), ArrowError> {
    match (arrow_type, dest.sql_type) {
        // Core MVP types
        (DataType::Int32, SqlDbType::Int) => Ok(()),
        (DataType::Int64, SqlDbType::BigInt) => Ok(()),
        (DataType::Float64, SqlDbType::Float) => Ok(()),
        (DataType::Utf8 | DataType::LargeUtf8, SqlDbType::NVarChar | SqlDbType::NChar) => Ok(()),
        (DataType::Decimal128(p, _), SqlDbType::Decimal | SqlDbType::Numeric) => {
            if *p > 38 {
                return Err(ArrowError::PrecisionOverflow {
                    column_name: column_name.to_string(),
                    precision: *p,
                });
            }
            Ok(())
        }

        // Extended types (Phase 6)
        (DataType::Boolean, SqlDbType::Bit) => Ok(()),
        (DataType::UInt8, SqlDbType::TinyInt) => Ok(()),
        (DataType::Int16, SqlDbType::SmallInt) => Ok(()),
        (DataType::Float32, SqlDbType::Real) => Ok(()),
        (DataType::Binary | DataType::LargeBinary, SqlDbType::VarBinary | SqlDbType::Binary) => {
            Ok(())
        }
        (DataType::Date32, SqlDbType::Date) => Ok(()),
        (DataType::Time64(arrow_schema::TimeUnit::Microsecond), SqlDbType::Time) => Ok(()),
        (DataType::Timestamp(arrow_schema::TimeUnit::Microsecond, None), SqlDbType::DateTime2) => {
            Ok(())
        }
        (
            DataType::Timestamp(arrow_schema::TimeUnit::Microsecond, Some(_)),
            SqlDbType::DateTimeOffset,
        ) => Ok(()),
        (DataType::FixedSizeBinary(16), SqlDbType::UniqueIdentifier) => Ok(()),
        (DataType::Utf8 | DataType::LargeUtf8, SqlDbType::Xml) => Ok(()),
        (DataType::Utf8 | DataType::LargeUtf8, SqlDbType::Json) => Ok(()),

        // Unsupported combination
        _ => Err(ArrowError::TypeMismatch {
            column_name: column_name.to_string(),
            arrow_type: arrow_type.clone(),
            sql_type: dest.sql_type,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_schema::{Field, Schema};
    use mssql_tds::datatypes::bulk_copy_metadata::TypeLength;

    fn make_dest_col(name: &str, sql_type: SqlDbType) -> BulkCopyColumnMetadata {
        BulkCopyColumnMetadata::new(name, sql_type, 0)
    }

    #[test]
    fn resolve_core_types() {
        let schema = Schema::new(vec![
            Field::new("id", DataType::Int32, false),
            Field::new("amount", DataType::Int64, false),
            Field::new("price", DataType::Float64, false),
            Field::new("name", DataType::Utf8, true),
            Field::new("total", DataType::Decimal128(18, 2), false),
        ]);

        let dest = vec![
            make_dest_col("id", SqlDbType::Int),
            make_dest_col("amount", SqlDbType::BigInt),
            make_dest_col("price", SqlDbType::Float),
            BulkCopyColumnMetadata::new("name", SqlDbType::NVarChar, 0)
                .with_length(200, TypeLength::Variable(200)),
            BulkCopyColumnMetadata::new("total", SqlDbType::Decimal, 0).with_precision_scale(18, 2),
        ];

        let registry = TypeMappingRegistry::resolve(&schema, &dest).unwrap();
        assert_eq!(registry.mappings.len(), 5);
        assert_eq!(registry.mappings[0].sql_type, SqlDbType::Int);
        assert_eq!(registry.mappings[4].precision, 18);
    }

    #[test]
    fn reject_empty_schema() {
        let schema = Schema::new(Vec::<Field>::new());
        let dest: Vec<BulkCopyColumnMetadata> = vec![];
        let err = TypeMappingRegistry::resolve(&schema, &dest).unwrap_err();
        assert!(matches!(err, ArrowError::EmptySchema));
    }

    #[test]
    fn reject_column_count_mismatch() {
        let schema = Schema::new(vec![
            Field::new("a", DataType::Int32, false),
            Field::new("b", DataType::Int64, false),
        ]);
        let dest = vec![make_dest_col("a", SqlDbType::Int)];
        let err = TypeMappingRegistry::resolve(&schema, &dest).unwrap_err();
        assert!(matches!(err, ArrowError::ColumnCountMismatch { .. }));
    }

    #[test]
    fn reject_type_mismatch() {
        let schema = Schema::new(vec![Field::new("x", DataType::Int32, false)]);
        let dest = vec![make_dest_col("x", SqlDbType::NVarChar)];
        let err = TypeMappingRegistry::resolve(&schema, &dest).unwrap_err();
        assert!(matches!(err, ArrowError::TypeMismatch { .. }));
    }

    #[test]
    fn reject_precision_overflow() {
        let schema = Schema::new(vec![Field::new("d", DataType::Decimal128(39, 0), false)]);
        let dest = vec![
            BulkCopyColumnMetadata::new("d", SqlDbType::Decimal, 0).with_precision_scale(38, 0),
        ];
        let err = TypeMappingRegistry::resolve(&schema, &dest).unwrap_err();
        assert!(matches!(err, ArrowError::PrecisionOverflow { .. }));
    }

    #[test]
    fn resolve_by_name_case_insensitive() {
        let schema = Schema::new(vec![
            Field::new("Price", DataType::Float64, false),
            Field::new("ID", DataType::Int32, false),
        ]);
        let dest = vec![
            make_dest_col("id", SqlDbType::Int),
            make_dest_col("price", SqlDbType::Float),
        ];
        let registry = TypeMappingRegistry::resolve_by_name(&schema, &dest).unwrap();
        assert_eq!(registry.mappings.len(), 2);
        // "Price" → dest index 1 (price), "ID" → dest index 0 (id)
        assert_eq!(registry.mappings[0].source_index, 0);
        assert_eq!(registry.mappings[0].dest_index, 1);
        assert_eq!(registry.mappings[1].source_index, 1);
        assert_eq!(registry.mappings[1].dest_index, 0);
    }

    #[test]
    fn resolve_by_name_unmatched_arrow_field() {
        let schema = Schema::new(vec![
            Field::new("id", DataType::Int32, false),
            Field::new("missing", DataType::Utf8, false),
        ]);
        let dest = vec![make_dest_col("id", SqlDbType::Int)];
        let err = TypeMappingRegistry::resolve_by_name(&schema, &dest).unwrap_err();
        assert!(matches!(err, ArrowError::TypeMismatch { .. }));
    }

    #[test]
    fn resolve_by_name_fewer_arrow_than_dest() {
        let schema = Schema::new(vec![Field::new("id", DataType::Int32, false)]);
        let dest = vec![
            make_dest_col("id", SqlDbType::Int),
            make_dest_col("optional_col", SqlDbType::NVarChar),
        ];
        // Arrow has fewer columns — only matches "id", skips "optional_col"
        let registry = TypeMappingRegistry::resolve_by_name(&schema, &dest).unwrap();
        assert_eq!(registry.mappings.len(), 1);
        assert_eq!(registry.mappings[0].dest_index, 0);
    }
}
