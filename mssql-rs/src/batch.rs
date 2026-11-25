// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::sync::Arc;

use mssql_tds::connection::tds_client::{ResultSet, StatementResult, TdsClient};

use crate::operation::{Operation, ensure_usable};
use crate::row::{DriverRowWriter, RowSchema};
use crate::{Error, ExecuteOptions, Result, ResultMetadata, Row};

/// An event from a raw SQL batch, in server response order.
#[derive(Debug)]
#[non_exhaustive]
pub enum BatchItem {
    /// Starts a rowset, including one that returns no rows.
    ResultSetStart(ResultMetadata),
    /// A decoded row from the current rowset.
    Row(Row),
    /// Ends the current rowset successfully.
    ResultSetEnd {
        /// The server-reported DONE count, which is not necessarily a DML count.
        /// `None` means the server did not send a count.
        row_count: Option<u64>,
    },
    /// A statement without a rowset that reported an update count or a message.
    NoRows {
        /// An affected-row count provided by the server. `None` is not zero.
        rows_affected: Option<u64>,
    },
}

enum State<'a> {
    Start(String, ExecuteOptions<'a>),
    Rows(DriverRowWriter),
    Advance,
    Done,
}

/// An incremental response from [`Client::simple_query_batch`](crate::Client::simple_query_batch).
///
/// Holds an exclusive borrow of the connection. Dropping between reads leaves
/// unread results for the next operation to drain; it does not send ATTENTION.
/// Use [`cancel`](Self::cancel) to explicitly discard the rest of the response.
#[must_use = "the batch is not sent until next_item is awaited"]
pub struct BatchStream<'a> {
    inner: &'a mut TdsClient,
    state: State<'a>,
    result_index: usize,
    continue_on_error: bool,
}

impl<'a> BatchStream<'a> {
    pub(crate) fn new(inner: &'a mut TdsClient, sql: String, options: ExecuteOptions<'a>) -> Self {
        let continue_on_error = options.on_error == crate::BatchErrorMode::Continue;
        Self {
            inner,
            state: State::Start(sql, options),
            result_index: 0,
            continue_on_error,
        }
    }

    /// Reads the next event, or `Ok(None)` after completion or a terminal error.
    ///
    /// The first call sends the batch. At most one decoded row is delivered at
    /// a time, but a single row (including any large values) is materialized
    /// before delivery. Rowset indices do not include `NoRows` events.
    ///
    /// With [`BatchErrorMode::Continue`](crate::BatchErrorMode::Continue), a
    /// nonfatal statement error may be followed by more results. An error
    /// ending a rowset replaces its `ResultSetEnd` event. Other errors end
    /// iteration. Dropping an in-flight read marks the connection dead; use
    /// native request options to time out while still completing cleanup.
    pub async fn next_item(&mut self) -> Result<Option<BatchItem>> {
        let result = match std::mem::replace(&mut self.state, State::Done) {
            State::Start(sql, options) => {
                let mut operation = Operation::new(self.inner)?;
                let result = operation.close_query().await.map_err(Error::Tds);
                operation.complete(result)?;

                let mut operation = Operation::new(self.inner)?;
                let result = Box::pin(operation.execute(sql, options)).await;
                operation.complete(result)
            }
            State::Advance => {
                let mut operation = Operation::new(self.inner)?;
                let result = operation.advance().await;
                operation.complete(result)
            }
            State::Rows(mut writer) => {
                let mut operation = Operation::new(self.inner)?;
                let result = operation.next_row_into(&mut writer).await;
                return match operation.complete(result) {
                    Ok(true) => {
                        let row = writer.take_row();
                        self.state = State::Rows(writer);
                        Ok(Some(BatchItem::Row(row)))
                    }
                    Ok(false) => {
                        self.state = State::Advance;
                        Ok(Some(BatchItem::ResultSetEnd {
                            row_count: self.inner.last_result_row_count(),
                        }))
                    }
                    Err(error) => self.statement_error(error),
                };
            }
            State::Done => return Ok(None),
        };
        match result {
            Ok(StatementResult::Rows) => {
                let schema = RowSchema::from_metadata(self.inner.get_metadata());
                let index = self.result_index;
                self.result_index += 1;
                let metadata = ResultMetadata::new(Arc::clone(&schema), index);
                self.state = State::Rows(DriverRowWriter::with_result_index(schema, index));
                Ok(Some(BatchItem::ResultSetStart(metadata)))
            }
            Ok(StatementResult::NoRows { rows_affected }) => {
                self.state = State::Advance;
                Ok(Some(BatchItem::NoRows { rows_affected }))
            }
            Ok(StatementResult::End) => Ok(None),
            Err(error) => self.statement_error(error),
        }
    }

    fn statement_error<T>(&mut self, error: mssql_tds::error::Error) -> Result<T> {
        if self.continue_on_error
            && matches!(error, mssql_tds::error::Error::SqlServerError { .. })
            && self.inner.has_open_batch()
            && !self.inner.is_connection_dead()
        {
            self.state = State::Advance;
        }
        Err(Error::Tds(error))
    }

    /// Discards the unread response, sending ATTENTION if it is still arriving.
    ///
    /// `timeout_secs` bounds cancellation settlement, separately from the
    /// request timeout. Zero uses the native default. An unstarted batch
    /// sends nothing. If the final packet is already buffered, the unread
    /// tail is discarded locally without sending ATTENTION.
    ///
    /// Await this call through completion. Failed or interrupted settlement
    /// retires the connection. Cancellation does not roll back SQL effects.
    pub async fn cancel(self, timeout_secs: u32) -> Result<()> {
        if matches!(self.state, State::Start(..)) {
            return Ok(());
        }
        ensure_usable(self.inner)?;
        if !self.inner.has_open_batch() {
            return Ok(());
        }
        let mut operation = Operation::new(self.inner)?;
        // Only keep the connection if settlement completed successfully.
        operation
            .abandon_query(timeout_secs)
            .await
            .map_err(Error::Tds)?;
        operation.complete(Ok(()))
    }
}
