// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::Arc;

use adbc_core::error::Result;
use adbc_core::options::{OptionStatement, OptionValue};
use adbc_core::{Optionable, PartitionedResult, Statement};
use arrow_array::{RecordBatch, RecordBatchReader};
use arrow_schema::{ArrowError, Schema, SchemaRef};
use mssql_tds::connection::tds_client::{PreparedStatement, ResultSet, StatementResult};
use mssql_tds::core::CancelHandle;

use crate::driver::{Cancellation, Reader, Shared, unsupported_getters};
use crate::error::{state, tds, unsupported};
use crate::options;
use crate::parameters::{self, Binding, ParameterCursor};
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
    pub(crate) binding: Option<Binding>,
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
            binding: None,
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
        self.shared.with(|_, session| {
            session.release_prepared(self.id);
            Ok(())
        })?;
        self.sql = Some(query.as_ref().to_owned());
        self.binding = None;
        Ok(())
    }
    fn set_substrait_plan(&mut self, _plan: impl AsRef<[u8]>) -> Result<()> {
        Err(unsupported("Substrait"))
    }
    fn prepare(&mut self) -> Result<()> {
        self.invalidate()?;
        let sql = self
            .sql
            .as_ref()
            .ok_or_else(|| state("SQL query not set"))?;
        self.shared.with(|_, session| {
            session.release_prepared(self.id);
            session
                .prepared
                .insert(self.id, PreparedStatement::new(sql));
            Ok(())
        })?;
        Ok(())
    }
    fn get_parameter_schema(&self) -> Result<Schema> {
        self.shared.with(|_, session| {
            if !session.prepared.contains_key(&self.id) {
                return Err(state("prepare the statement before describing parameters"));
            }
            Ok(())
        })?;
        if let Some(binding) = &self.binding {
            return Ok(Schema::new(
                binding
                    .schema()
                    .fields()
                    .iter()
                    .enumerate()
                    .map(|(index, field)| {
                        field.as_ref().clone().with_name(format!("@p{}", index + 1))
                    })
                    .collect::<Vec<_>>(),
            ));
        }
        crate::metadata::describe(
            &self.shared,
            self.sql
                .as_deref()
                .ok_or_else(|| state("SQL query not set"))?,
            None,
            true,
        )
    }
    fn bind(&mut self, batch: RecordBatch) -> Result<()> {
        parameters::validate(&batch.schema())?;
        self.invalidate()?;
        self.binding = Some(Binding::Batch(batch));
        Ok(())
    }
    fn bind_stream(&mut self, reader: Box<dyn RecordBatchReader + Send>) -> Result<()> {
        let schema = reader.schema();
        parameters::validate(&schema)?;
        self.invalidate()?;
        self.binding = Some(Binding::Stream {
            schema,
            reader: Some(reader),
        });
        Ok(())
    }
    fn execute_schema(&mut self) -> Result<Schema> {
        self.invalidate()?;
        let bound = self.binding.as_ref().map(Binding::schema);
        crate::metadata::describe(
            &self.shared,
            self.sql
                .as_deref()
                .ok_or_else(|| state("SQL query not set"))?,
            bound.as_deref(),
            false,
        )
    }
    fn execute_partitions(&mut self) -> Result<PartitionedResult> {
        Err(unsupported("partitioned execution"))
    }
    fn get_cancel_handle(&self) -> Box<dyn adbc_core::CancelHandle> {
        Box::new(self.cancel.clone())
    }

    fn execute(&mut self) -> Result<Reader> {
        let sql = self.sql.clone().ok_or_else(|| state("SQL query not set"))?;
        let mut cursor = ParameterCursor::new(self.binding.as_mut())?;
        let Some(params) = cursor.next()? else {
            let schema = Arc::new(self.execute_schema()?);
            return Ok(Box::new(arrow_array::RecordBatchIterator::new(
                std::iter::empty(),
                schema,
            )));
        };
        let handle = Arc::new(CancelHandle::new());
        self.cancel.set(Some(Arc::clone(&handle)))?;
        let result = self.shared.with(|executor, session| {
            executor.run(async {
                let generation = session.start(self.id, self.timeout).await?;
                self.shared.cancel.set(Some(Arc::clone(&handle)))?;
                let mut result = session
                    .request(self.id, &sql, params, self.timeout, &handle)
                    .await?;
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
                    handle: Arc::clone(&handle),
                    sql,
                    cursor,
                    timeout: self.timeout,
                    done,
                })
            })
        });
        if result.is_err() {
            self.cancel.clear(&handle)?;
            self.shared.cancel.clear(&handle)?;
        }
        Ok(Box::new(result?))
    }

    fn execute_update(&mut self) -> Result<Option<i64>> {
        let sql = self.sql.clone().ok_or_else(|| state("SQL query not set"))?;
        let mut cursor = ParameterCursor::new(self.binding.as_mut())?;
        let handle = Arc::new(CancelHandle::new());
        self.cancel.set(Some(Arc::clone(&handle)))?;
        let result =
            self.shared.with(|executor, session| {
                executor.run(async {
                    session.start(self.id, self.timeout).await?;
                    self.shared.cancel.set(Some(Arc::clone(&handle)))?;
                    let mut count: Option<i64> = None;
                    while let Some(params) = cursor.next()? {
                        let mut result = session
                            .request(self.id, &sql, params, self.timeout, &handle)
                            .await?;
                        loop {
                            match result {
                                StatementResult::Rows => {
                                    session.abandon().await?;
                                    return Err(state(
                                        "execute_update received a row result; use execute",
                                    ));
                                }
                                StatementResult::NoRows {
                                    rows_affected: Some(rows),
                                } => {
                                    let rows = i64::try_from(rows)
                                        .map_err(|_| crate::error::data("row count overflow"))?;
                                    count =
                                        Some(count.unwrap_or(0).checked_add(rows).ok_or_else(
                                            || crate::error::data("row count overflow"),
                                        )?);
                                }
                                StatementResult::End => break,
                                _ => {}
                            }
                            result = session.client.advance().await.map_err(tds)?;
                        }
                    }
                    Ok(count)
                })
            });
        self.cancel.clear(&handle)?;
        self.shared.cancel.clear(&handle)?;
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
    handle: Arc<CancelHandle>,
    sql: String,
    cursor: ParameterCursor,
    timeout: u32,
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
                            if let Some(params) = self.cursor.next()? {
                                let mut result = session
                                    .request(
                                        self.owner.0,
                                        &self.sql,
                                        params,
                                        self.timeout,
                                        &self.handle,
                                    )
                                    .await?;
                                while matches!(result, StatementResult::NoRows { .. }) {
                                    result = session.client.advance().await.map_err(tds)?;
                                }
                                if result != StatementResult::Rows {
                                    return Err(state(
                                        "parameterized query did not return a tabular result",
                                    ));
                                }
                                if types::schema(session.client.get_metadata())? != self.schema {
                                    return Err(crate::error::data(
                                        "result schema changed across parameter rows",
                                    ));
                                }
                                continue;
                            }
                            session.active = None;
                            self.done = true;
                            self.cancel.clear(&self.handle)?;
                            self.shared.cancel.clear(&self.handle)?;
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
                    .clear(&self.handle)
                    .map_err(|error| tracing::error!(%error, "ADBC cancellation cleanup"));
                if let Err(error) = self.shared.cancel.clear(&self.handle) {
                    tracing::error!(%error, "ADBC connection cancellation cleanup");
                }
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
                self.cancel.clear(&self.handle)?;
                self.shared.cancel.clear(&self.handle)?;
            }

            Ok(())
        });
        if let Err(error) = result {
            tracing::error!(%error, "ADBC dropped stream cleanup failed");
        }
    }
}

impl Drop for MssqlStatement {
    fn drop(&mut self) {
        if let Err(error) = self.shared.with(|_, session| {
            session.release_prepared(self.id);
            Ok(())
        }) {
            tracing::error!(%error, "ADBC prepared statement release deferred to connection closure");
        }
    }
}
