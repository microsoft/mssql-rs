// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::Arc;

use adbc_core::error::Result;
use adbc_core::options::{OptionStatement, OptionValue};
use adbc_core::{Optionable, PartitionedResult, Statement};
use arrow_array::{RecordBatch, RecordBatchReader};
use arrow_schema::{ArrowError, Schema, SchemaRef};
use mssql_tds::connection::tds_client::{ExecuteOptions, ResultSet, StatementResult};
use mssql_tds::core::CancelHandle;

use crate::driver::{Cancellation, Reader, Shared, unsupported_getters};
use crate::error::{state, tds, unsupported};
use crate::options;
use crate::types::{self, ArrowRowWriter};

/// Reusable ADBC statement. SQL Server parameters use native `@p1`, `@p2`, ... names.
pub struct MssqlStatement {
    pub(crate) shared: Arc<Shared>,
    pub(crate) id: u64,
    pub(crate) sql: Option<String>,
    pub(crate) cancel: Cancellation,
    pub(crate) timeout: u32,
    pub(crate) batch_rows: usize,
    pub(crate) batch_bytes: usize,
}

impl MssqlStatement {
    pub(crate) fn new(shared: Arc<Shared>, id: u64) -> Self {
        Self {
            id,
            sql: None,
            cancel: Cancellation::default(),
            timeout: shared.config.timeout,
            batch_rows: shared.config.batch_rows,
            batch_bytes: shared.config.batch_bytes,
            shared,
        }
    }
    pub(crate) fn invalidate(&mut self) -> Result<()> {
        self.shared.with(|executor, session| {
            if session.active.is_some_and(|(owner, _)| owner == self.id) {
                executor.run(session.abandon())?;
                self.cancel.set(None)?;
                self.shared.cancel.set(None)?;
            }
            Ok(())
        })
    }
}

impl Optionable for MssqlStatement {
    type Option = OptionStatement;
    fn set_option(&mut self, key: OptionStatement, value: OptionValue) -> Result<()> {
        match key.as_ref() {
            options::QUERY_TIMEOUT => {
                self.timeout = options::integer(value, u32::MAX as usize)? as u32
            }
            options::BATCH_ROWS => {
                let value = options::integer(value, 1_000_000)?;
                if value == 0 {
                    return Err(crate::error::invalid("batch_rows must be positive"));
                }
                self.batch_rows = value;
            }
            options::BATCH_BYTES => {
                let value = options::integer(value, 1024 * 1024 * 1024)?;
                if value == 0 {
                    return Err(crate::error::invalid("batch_bytes must be positive"));
                }
                self.batch_bytes = value;
            }
            _ => return Err(unsupported(format!("statement option {}", key.as_ref()))),
        }
        Ok(())
    }
    fn get_option_string(&self, key: OptionStatement) -> Result<String> {
        Err(unsupported(key.as_ref()))
    }
    fn get_option_int(&self, key: OptionStatement) -> Result<i64> {
        match key.as_ref() {
            options::QUERY_TIMEOUT => Ok(i64::from(self.timeout)),
            options::BATCH_ROWS => Ok(self.batch_rows as i64),
            options::BATCH_BYTES => Ok(self.batch_bytes as i64),
            _ => Err(unsupported(key.as_ref())),
        }
    }
    unsupported_getters!(OptionStatement);
}

impl Statement for MssqlStatement {
    fn set_sql_query(&mut self, query: impl AsRef<str>) -> Result<()> {
        self.invalidate()?;
        self.sql = Some(query.as_ref().to_owned());
        Ok(())
    }
    fn set_substrait_plan(&mut self, _plan: impl AsRef<[u8]>) -> Result<()> {
        Err(unsupported("Substrait"))
    }
    fn prepare(&mut self) -> Result<()> {
        self.invalidate()?;
        self.sql
            .as_ref()
            .ok_or_else(|| state("SQL query not set"))?;
        Ok(())
    }
    fn get_parameter_schema(&self) -> Result<Schema> {
        Err(unsupported("parameter description"))
    }
    fn bind(&mut self, _batch: RecordBatch) -> Result<()> {
        Err(unsupported("parameter binding"))
    }
    fn bind_stream(&mut self, _reader: Box<dyn RecordBatchReader + Send>) -> Result<()> {
        Err(unsupported("stream binding"))
    }
    fn execute_schema(&mut self) -> Result<Schema> {
        Err(unsupported("schema description"))
    }
    fn execute_partitions(&mut self) -> Result<PartitionedResult> {
        Err(unsupported("partitioned execution"))
    }
    fn get_cancel_handle(&self) -> Box<dyn adbc_core::CancelHandle> {
        Box::new(self.cancel.clone())
    }

    fn execute(&mut self) -> Result<Reader> {
        let sql = self.sql.clone().ok_or_else(|| state("SQL query not set"))?;
        let handle = Arc::new(CancelHandle::new());
        self.cancel.set(Some(Arc::clone(&handle)))?;
        let result = self.shared.with(|executor, session| {
            self.shared.cancel.set(Some(Arc::clone(&handle)))?;
            executor.run(async {
                let generation = session.start(self.id, self.timeout).await?;
                let mut result = session
                    .client
                    .execute(
                        sql,
                        ExecuteOptions::new()
                            .timeout_secs(self.timeout)
                            .cancel(&handle),
                    )
                    .await
                    .map_err(tds)?;
                while matches!(result, StatementResult::NoRows { .. }) {
                    result = session.client.advance().await.map_err(tds)?;
                }
                let schema = if result == StatementResult::Rows {
                    match types::schema(session.client.get_metadata()) {
                        Ok(schema) => schema,
                        Err(error) => {
                            session.abandon().await?;
                            return Err(error);
                        }
                    }
                } else {
                    Arc::new(Schema::empty())
                };
                let done = result == StatementResult::End;
                if !done {
                    session.active = Some((self.id, generation));
                }
                Ok(QueryReader {
                    shared: Arc::clone(&self.shared),
                    owner: (self.id, generation),
                    schema,
                    batch_rows: self.batch_rows,
                    batch_bytes: self.batch_bytes,
                    cancel: self.cancel.clone(),
                    done,
                })
            })
        });
        if result.is_err() {
            self.cancel.set(None)?;
            self.shared.cancel.set(None)?;
        }
        Ok(Box::new(result?))
    }

    fn execute_update(&mut self) -> Result<Option<i64>> {
        let sql = self.sql.clone().ok_or_else(|| state("SQL query not set"))?;
        let handle = Arc::new(CancelHandle::new());
        self.cancel.set(Some(Arc::clone(&handle)))?;
        let result = self.shared.with(|executor, session| {
            self.shared.cancel.set(Some(Arc::clone(&handle)))?;
            executor.run(async {
                session.start(self.id, self.timeout).await?;
                let mut result = session
                    .client
                    .execute(
                        sql,
                        ExecuteOptions::new()
                            .timeout_secs(self.timeout)
                            .cancel(&handle),
                    )
                    .await
                    .map_err(tds)?;
                let mut count: Option<i64> = None;
                loop {
                    match result {
                        StatementResult::Rows => {
                            session.abandon().await?;
                            return Err(state("execute_update received a row result; use execute"));
                        }
                        StatementResult::NoRows {
                            rows_affected: Some(rows),
                        } => {
                            let rows = i64::try_from(rows)
                                .map_err(|_| crate::error::data("row count overflow"))?;
                            count = Some(
                                count
                                    .unwrap_or(0)
                                    .checked_add(rows)
                                    .ok_or_else(|| crate::error::data("row count overflow"))?,
                            );
                        }
                        StatementResult::End => return Ok(count),
                        _ => {}
                    }
                    result = session.client.advance().await.map_err(tds)?;
                }
            })
        });
        self.cancel.set(None)?;
        self.shared.cancel.set(None)?;
        result
    }
}

struct QueryReader {
    shared: Arc<Shared>,
    owner: (u64, u64),
    schema: SchemaRef,
    batch_rows: usize,
    batch_bytes: usize,
    cancel: Cancellation,
    done: bool,
}

impl QueryReader {
    fn fetch(&mut self) -> Result<Option<RecordBatch>> {
        self.shared.with(|executor, session| {
            if session.active != Some(self.owner) {
                return Err(state("result stream invalidated"));
            }
            executor.run(async {
                let mut writer = ArrowRowWriter::new(Arc::clone(&self.schema), self.batch_rows);
                while writer.rows < self.batch_rows && writer.bytes < self.batch_bytes {
                    match session.client.next_row_into(&mut writer).await {
                        Ok(true) => {
                            if let Some(error) = writer.error.take() {
                                session.abandon().await?;
                                return Err(error);
                            }
                        }
                        Ok(false) => {
                            let mut result = session.client.advance().await.map_err(tds)?;
                            while matches!(result, StatementResult::NoRows { .. }) {
                                result = session.client.advance().await.map_err(tds)?;
                            }
                            if result == StatementResult::Rows {
                                session.abandon().await?;
                                return Err(unsupported("multiple tabular result sets"));
                            }
                            session.active = None;
                            self.done = true;
                            self.cancel.set(None)?;
                            self.shared.cancel.set(None)?;
                            break;
                        }
                        Err(error) => return Err(tds(error)),
                    }
                }
                if writer.rows == 0 {
                    Ok(None)
                } else {
                    Ok(Some(writer.finish()?))
                }
            })
        })
    }
}

impl Iterator for QueryReader {
    type Item = std::result::Result<RecordBatch, ArrowError>;
    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        match self.fetch() {
            Ok(batch) => batch.map(Ok),
            Err(error) => {
                self.done = true;
                let cleanup = self.shared.with(|executor, session| {
                    if session.active == Some(self.owner) {
                        executor.run(session.abandon())?;
                    }
                    Ok(())
                });
                if let Err(error) = cleanup {
                    tracing::error!(%error, "ADBC failed stream cleanup");
                }
                let _ = self
                    .cancel
                    .set(None)
                    .map_err(|error| tracing::error!(%error, "ADBC cancellation cleanup"));
                Some(Err(ArrowError::ExternalError(Box::new(error))))
            }
        }
    }
}

impl RecordBatchReader for QueryReader {
    fn schema(&self) -> SchemaRef {
        Arc::clone(&self.schema)
    }
}

impl Drop for QueryReader {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        let result = self.shared.with(|executor, session| {
            if session.active == Some(self.owner) {
                executor.run(session.abandon())?;
                self.cancel.set(None)?;
                self.shared.cancel.set(None)?;
            }
            Ok(())
        });
        if let Err(error) = result {
            tracing::error!(%error, "ADBC dropped stream cleanup failed");
        }
    }
}
