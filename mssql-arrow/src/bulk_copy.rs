// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::time::Duration;

use arrow_array::RecordBatch;
use async_trait::async_trait;
use tracing::instrument;

use mssql_tds::connection::bulk_copy::{
    BulkCopy, BulkCopyProgress, BulkCopyResult, BulkLoadRow, ColumnMapping,
};
use mssql_tds::connection::tds_client::TdsClient;
use mssql_tds::core::TdsResult;
use mssql_tds::message::bulk_load::StreamingBulkLoadWriter;

use crate::serializer;
use crate::type_mapping::TypeMappingRegistry;

/// Pre-serialized row whose TDS column bytes are ready for wire transmission.
pub(crate) struct ArrowBulkLoadRow {
    tds_bytes: Vec<u8>,
    col_count: usize,
}

#[async_trait]
impl BulkLoadRow for ArrowBulkLoadRow {
    async fn write_to_packet(
        &self,
        writer: &mut StreamingBulkLoadWriter<'_>,
        column_index: &mut usize,
    ) -> TdsResult<()> {
        writer.write_raw_bytes(&self.tds_bytes).await?;
        *column_index += self.col_count;
        Ok(())
    }
}

#[async_trait]
impl BulkLoadRow for &ArrowBulkLoadRow {
    async fn write_to_packet(
        &self,
        writer: &mut StreamingBulkLoadWriter<'_>,
        column_index: &mut usize,
    ) -> TdsResult<()> {
        writer.write_raw_bytes(&self.tds_bytes).await?;
        *column_index += self.col_count;
        Ok(())
    }
}

/// Builder for bulk inserting Arrow RecordBatch data into SQL Server.
pub struct ArrowBulkCopy<'a> {
    client: &'a mut TdsClient,
    table_name: String,
    batch_size: usize,
    timeout: Option<Duration>,
    check_constraints: bool,
    fire_triggers: bool,
    keep_identity: bool,
    keep_nulls: bool,
    table_lock: bool,
    use_internal_transaction: bool,
    column_mappings: Vec<ColumnMapping>,
    progress_callback: Option<Box<dyn FnMut(BulkCopyProgress) + Send + 'a>>,
}

impl<'a> ArrowBulkCopy<'a> {
    pub fn new(client: &'a mut TdsClient, table_name: impl Into<String>) -> Self {
        Self {
            client,
            table_name: table_name.into(),
            batch_size: 0,
            timeout: None,
            check_constraints: false,
            fire_triggers: false,
            keep_identity: false,
            keep_nulls: false,
            table_lock: false,
            use_internal_transaction: false,
            column_mappings: Vec::new(),
            progress_callback: None,
        }
    }

    pub fn batch_size(mut self, size: usize) -> Self {
        self.batch_size = size;
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn check_constraints(mut self, enabled: bool) -> Self {
        self.check_constraints = enabled;
        self
    }

    pub fn fire_triggers(mut self, enabled: bool) -> Self {
        self.fire_triggers = enabled;
        self
    }

    pub fn keep_identity(mut self, enabled: bool) -> Self {
        self.keep_identity = enabled;
        self
    }

    pub fn keep_nulls(mut self, enabled: bool) -> Self {
        self.keep_nulls = enabled;
        self
    }

    pub fn table_lock(mut self, enabled: bool) -> Self {
        self.table_lock = enabled;
        self
    }

    pub fn use_internal_transaction(mut self, enabled: bool) -> Self {
        self.use_internal_transaction = enabled;
        self
    }

    pub fn add_column_mapping(mut self, mapping: ColumnMapping) -> Self {
        self.column_mappings.push(mapping);
        self
    }

    pub fn on_progress<F>(mut self, callback: F) -> Self
    where
        F: FnMut(BulkCopyProgress) + Send + 'a,
    {
        self.progress_callback = Some(Box::new(callback));
        self
    }

    /// Bulk insert a single RecordBatch.
    #[instrument(skip_all, fields(table = %self.table_name, rows = batch.num_rows()))]
    pub async fn write_batch(&mut self, batch: &RecordBatch) -> TdsResult<BulkCopyResult> {
        let mut bulk_copy = self.build_bulk_copy();

        let dest_metadata = bulk_copy.retrieve_destination_metadata().await?;
        let registry = TypeMappingRegistry::resolve(batch.schema().as_ref(), &dest_metadata)?;

        let col_count = registry.mappings.len();
        let row_buffers = serializer::serialize_batch(batch, &registry)?;

        let rows: Vec<ArrowBulkLoadRow> = row_buffers
            .into_iter()
            .map(|tds_bytes| ArrowBulkLoadRow {
                tds_bytes,
                col_count,
            })
            .collect();

        let result = bulk_copy.write_to_server_zerocopy(&rows).await?;
        Ok(result)
    }

    /// Bulk insert multiple RecordBatches in a single session.
    #[instrument(skip_all, fields(table = %self.table_name, batch_count = batches.len()))]
    pub async fn write_batches(&mut self, batches: &[RecordBatch]) -> TdsResult<BulkCopyResult> {
        if batches.is_empty() {
            return Ok(BulkCopyResult::new(0, Duration::ZERO));
        }

        let first_schema = batches[0].schema();
        let mut bulk_copy = self.build_bulk_copy();
        let dest_metadata = bulk_copy.retrieve_destination_metadata().await?;
        let registry = TypeMappingRegistry::resolve(first_schema.as_ref(), &dest_metadata)?;

        let col_count = registry.mappings.len();
        let mut total_rows = 0u64;
        let start = std::time::Instant::now();

        for batch in batches {
            let row_buffers = serializer::serialize_batch(batch, &registry)?;
            let rows: Vec<ArrowBulkLoadRow> = row_buffers
                .into_iter()
                .map(|tds_bytes| ArrowBulkLoadRow {
                    tds_bytes,
                    col_count,
                })
                .collect();

            let result = bulk_copy.write_to_server_zerocopy(&rows).await?;
            total_rows += result.rows_affected;
        }

        Ok(BulkCopyResult::new(total_rows, start.elapsed()))
    }

    fn build_bulk_copy(&mut self) -> BulkCopy<'_> {
        let mut bc = BulkCopy::new(self.client, &self.table_name);

        if self.batch_size > 0 {
            bc = bc.batch_size(self.batch_size);
        }
        if let Some(timeout) = self.timeout {
            bc = bc.timeout(timeout);
        }
        if self.check_constraints {
            bc = bc.check_constraints(true);
        }
        if self.fire_triggers {
            bc = bc.fire_triggers(true);
        }
        if self.keep_identity {
            bc = bc.keep_identity(true);
        }
        if self.keep_nulls {
            bc = bc.keep_nulls(true);
        }
        if self.table_lock {
            bc = bc.table_lock(true);
        }
        if self.use_internal_transaction {
            bc = bc.use_internal_transaction(true);
        }
        for mapping in self.column_mappings.drain(..) {
            bc = bc.add_column_mapping(mapping);
        }
        if let Some(callback) = self.progress_callback.take() {
            bc = bc.on_progress(callback);
        }
        bc
    }
}
