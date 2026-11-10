// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::collections::HashSet;
use std::time::Duration;

use adbc_core::error::Result;
use adbc_core::options::IngestMode;
use arrow_array::{RecordBatch, RecordBatchReader};
use arrow_schema::{DataType, Schema};
use async_trait::async_trait;
use mssql_tds::connection::bulk_copy::{BulkCopy, BulkLoadRow, ColumnMapping};
use mssql_tds::connection::tds_client::{ExecuteOptions, StatementResult};
use mssql_tds::core::{CancelHandle, TdsResult};
use mssql_tds::datatypes::bulk_copy_metadata::{BulkCopyColumnMetadata, SqlDbType};
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::datatypes::sqltypes::SqlType;
use mssql_tds::escape_identifier;
use mssql_tds::message::bulk_load::StreamingBulkLoadWriter;

use crate::driver::Session;
use crate::error::{data, invalid, tds, unsupported};
use crate::parameters;

#[derive(Clone)]
pub(crate) struct Ingestion {
    pub(crate) table: Option<String>,
    pub(crate) catalog: Option<String>,
    pub(crate) schema: Option<String>,
    pub(crate) mode: IngestMode,
    pub(crate) temporary: bool,
}

impl Default for Ingestion {
    fn default() -> Self {
        Self {
            table: None,
            catalog: None,
            schema: None,
            mode: IngestMode::Create,
            temporary: false,
        }
    }
}

impl Ingestion {
    pub(crate) fn target(&self) -> Result<String> {
        let table = self
            .table
            .as_deref()
            .ok_or_else(|| invalid("target table is required"))?;
        if table.is_empty() || table.contains('\0') {
            return Err(invalid("invalid target table"));
        }
        if self.temporary {
            if self.catalog.is_some()
                || self.schema.is_some()
                || !table.starts_with('#')
                || table.starts_with("##")
            {
                return Err(invalid(
                    "temporary ingestion requires a local #table and no catalog or schema",
                ));
            }
            return Ok(escape_identifier(table));
        }
        if table.starts_with('#') {
            return Err(invalid("set temporary=true for a local #table"));
        }
        let mut parts = Vec::new();
        if let Some(catalog) = &self.catalog {
            parts.push(escape_identifier(catalog));
        }
        parts.push(escape_identifier(self.schema.as_deref().unwrap_or("dbo")));
        parts.push(escape_identifier(table));
        Ok(parts.join("."))
    }
}

fn create_sql(target: &str, schema: &Schema) -> Result<String> {
    if schema.fields().is_empty() {
        return Err(invalid("ingestion requires at least one column"));
    }
    let mut names = HashSet::new();
    let fields = schema
        .fields()
        .iter()
        .map(|field| {
            if field.name().is_empty()
                || field.name().contains('\0')
                || !names.insert(field.name().to_lowercase())
            {
                return Err(invalid(
                    "ingestion column names must be nonempty and unique",
                ));
            }
            Ok(format!(
                "{} {} {}",
                escape_identifier(field.name()),
                parameters::declaration(field.data_type())?,
                if field.is_nullable() {
                    "NULL"
                } else {
                    "NOT NULL"
                }
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(format!("CREATE TABLE {target} ({})", fields.join(",")))
}

async fn ddl(
    session: &mut Session,
    sql: String,
    timeout: u32,
    cancel: &CancelHandle,
) -> Result<()> {
    let mut result = session
        .client
        .execute(
            sql,
            ExecuteOptions::new().timeout_secs(timeout).cancel(cancel),
        )
        .await
        .map_err(tds)?;
    loop {
        match result {
            StatementResult::End => return Ok(()),
            StatementResult::Rows => return Err(data("ingestion DDL returned rows")),
            _ => {}
        }
        result = session.client.advance().await.map_err(tds)?;
    }
}

fn compatible(source: &DataType, target: &BulkCopyColumnMetadata) -> bool {
    use SqlDbType as S;
    match (source, target.sql_type) {
        (DataType::Boolean, S::Bit)
        | (DataType::UInt8, S::TinyInt)
        | (DataType::Int8 | DataType::Int16, S::SmallInt)
        | (DataType::UInt16 | DataType::Int32, S::Int)
        | (DataType::UInt32 | DataType::Int64, S::BigInt)
        | (DataType::Float32, S::Real)
        | (DataType::Float64, S::Float)
        | (DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View, S::NVarChar)
        | (DataType::Binary | DataType::LargeBinary | DataType::BinaryView, S::VarBinary)
        | (DataType::FixedSizeBinary(16), S::UniqueIdentifier)
        | (DataType::Date32, S::Date) => true,
        (DataType::Decimal128(p, s), S::Decimal | S::Numeric) => {
            *p == target.precision && *s == target.scale as i8
        }
        (DataType::UInt64, S::Decimal | S::Numeric) => target.precision == 20 && target.scale == 0,
        (_, S::Time | S::DateTime2 | S::DateTimeOffset) => {
            target.scale == 7
                && parameters::declaration(source).is_ok_and(|declaration| {
                    declaration
                        == match target.sql_type {
                            S::Time => "time(7)",
                            S::DateTime2 => "datetime2(7)",
                            _ => "datetimeoffset(7)",
                        }
                })
        }
        _ => false,
    }
}

fn cell(value: SqlType) -> TdsResult<ColumnValues> {
    macro_rules! value {
        ($value:expr, $variant:ident) => {
            $value
                .map(ColumnValues::$variant)
                .unwrap_or(ColumnValues::Null)
        };
    }
    Ok(match value {
        SqlType::Bit(v) => value!(v, Bit),
        SqlType::TinyInt(v) => value!(v, TinyInt),
        SqlType::SmallInt(v) => value!(v, SmallInt),
        SqlType::Int(v) => value!(v, Int),
        SqlType::BigInt(v) => value!(v, BigInt),
        SqlType::Real(v) => value!(v, Real),
        SqlType::Float(v) => value!(v, Float),
        SqlType::NVarcharMax(v) => value!(v, String),
        SqlType::VarBinaryMax(v) => value!(v, Bytes),
        SqlType::Decimal(v) => value!(v, Decimal),
        SqlType::Uuid(v) => value!(v, Uuid),
        SqlType::Date(v) => value!(v, Date),
        SqlType::Time(v) => value!(v, Time),
        SqlType::DateTime2(v) => value!(v, DateTime2),
        SqlType::DateTimeOffset(v) => value!(v, DateTimeOffset),
        _ => {
            return Err(mssql_tds::error::Error::TypeConversionError(
                "unsupported ingestion value".into(),
            ));
        }
    })
}

struct Row<'a> {
    batch: &'a RecordBatch,
    row: usize,
    columns: &'a [(usize, BulkCopyColumnMetadata)],
    cancel: &'a CancelHandle,
}

#[async_trait]
impl BulkLoadRow for Row<'_> {
    async fn write_to_packet(
        &self,
        writer: &mut StreamingBulkLoadWriter<'_>,
        column: &mut usize,
    ) -> TdsResult<()> {
        if self.cancel.is_cancelled() {
            return Err(mssql_tds::error::Error::OperationCancelledError(
                "ADBC ingestion cancelled".into(),
            ));
        }
        for (index, metadata) in self.columns {
            let value = cell(
                parameters::value(self.batch.column(*index).as_ref(), self.row).map_err(
                    |error| mssql_tds::error::Error::TypeConversionError(error.to_string()),
                )?,
            )?;
            if matches!(value, ColumnValues::Null) && !metadata.is_nullable {
                return Err(mssql_tds::error::Error::TypeConversionError(format!(
                    "NULL for non-null column {}",
                    metadata.column_name
                )));
            }
            let length = match &value {
                ColumnValues::String(value) => value.bytes.len() as i64,
                ColumnValues::Bytes(value) => value.len() as i64,
                _ => 0,
            };
            if metadata.length > 0 && length > i64::from(metadata.length) {
                return Err(mssql_tds::error::Error::TypeConversionError(format!(
                    "value exceeds length of column {}",
                    metadata.column_name
                )));
            }
            writer.write_column_value(*column, &value).await?;
            *column += 1;
        }
        Ok(())
    }
}

pub(crate) async fn execute(
    session: &mut Session,
    config: &Ingestion,
    mut reader: Box<dyn RecordBatchReader + Send>,
    timeout: u32,
    cancel: &CancelHandle,
) -> Result<i64> {
    let target = config.target()?;
    let schema = reader.schema();
    let create = create_sql(&target, &schema)?;
    let owned_transaction = session.autocommit;
    if owned_transaction {
        session
            .client
            .begin_transaction_with_options(
                session.isolation.clone(),
                None,
                ExecuteOptions::new().timeout_secs(timeout).cancel(cancel),
            )
            .await
            .map_err(tds)?;
    }
    let result = async {
        match config.mode {
            IngestMode::Create => ddl(session, create, timeout, cancel).await?,
            IngestMode::Replace => {
                ddl(
                    session,
                    format!("DROP TABLE IF EXISTS {target};{create}"),
                    timeout,
                    cancel,
                )
                .await?
            }
            IngestMode::CreateAppend => {
                let literal = target.replace('\'', "''");
                let object = if config.temporary {
                    format!("tempdb..{literal}")
                } else {
                    literal
                };
                ddl(
                    session,
                    format!("IF OBJECT_ID(N'{object}',N'U') IS NULL {create}"),
                    timeout,
                    cancel,
                )
                .await?;
            }
            IngestMode::Append => {}
        }
        let mut bulk = BulkCopy::new(&mut session.client, &target)
            .timeout(Duration::from_secs(u64::from(timeout)))
            .keep_nulls(true)
            .check_constraints(true)
            .fire_triggers(true);
        let metadata = bulk.retrieve_destination_metadata().await.map_err(tds)?;
        let mut columns = Vec::new();
        for metadata in metadata {
            if let Some((index, field)) = schema
                .fields()
                .iter()
                .enumerate()
                .find(|(_, field)| field.name() == &metadata.column_name)
            {
                if metadata.is_identity
                    || metadata.is_encrypted
                    || !compatible(field.data_type(), &metadata)
                {
                    return Err(unsupported(format!(
                        "ingestion into {} ({:?}) from {}",
                        metadata.column_name,
                        metadata.sql_type,
                        field.data_type()
                    )));
                }
                bulk = bulk.add_column_mapping(ColumnMapping::by_ordinal(index, field.name()));
                columns.push((index, metadata));
            }
        }
        if columns.len() != schema.fields().len() {
            return Err(invalid(
                "every input field must match a destination column exactly",
            ));
        }
        let mut count = 0_i64;
        for batch in &mut reader {
            let batch = batch.map_err(|error| data(format!("ingestion input failed: {error}")))?;
            if batch.schema() != schema {
                return Err(data("ingestion input schema changed"));
            }
            let result = bulk
                .write_to_server_zerocopy((0..batch.num_rows()).map(|row| Row {
                    batch: &batch,
                    row,
                    columns: &columns,
                    cancel,
                }))
                .await
                .map_err(tds)?;
            count = count
                .checked_add(
                    i64::try_from(result.rows_affected)
                        .map_err(|_| data("ingestion row count overflow"))?,
                )
                .ok_or_else(|| data("ingestion row count overflow"))?;
        }
        Ok(count)
    }
    .await;
    if result.is_err() && session.client.has_open_batch() {
        session.abandon().await?;
    }
    if owned_transaction {
        if result.is_ok() {
            let committed = session
                .client
                .commit_transaction_with_options(
                    None,
                    None,
                    ExecuteOptions::new().timeout_secs(timeout).cancel(cancel),
                )
                .await;
            if let Err(error) = committed {
                if session.client.has_open_batch() {
                    session.abandon().await?;
                }
                if session.client.has_active_transaction() {
                    session
                        .client
                        .rollback_transaction_with_options(
                            None,
                            None,
                            ExecuteOptions::new().timeout_secs(5),
                        )
                        .await
                        .map_err(tds)?;
                }
                return Err(tds(error));
            }
        } else if session.client.has_active_transaction() {
            session
                .client
                .rollback_transaction_with_options(
                    None,
                    None,
                    ExecuteOptions::new().timeout_secs(5),
                )
                .await
                .map_err(tds)?;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn identifiers_are_components_not_sql() {
        let config = Ingestion {
            table: Some("x]; DROP TABLE y--".into()),
            ..Default::default()
        };
        assert_eq!(config.target().unwrap(), "[dbo].[x]]; DROP TABLE y--]");
        assert!(
            Ingestion {
                table: Some("#x".into()),
                ..Default::default()
            }
            .target()
            .is_err()
        );
    }
}
