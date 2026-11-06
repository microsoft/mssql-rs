// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

//! Tiberius data-query compatibility over the bridge's existing connection.
//!
//! Use [`Client::query_compat`](crate::Client::query_compat) or
//! [`Client::simple_query_compat`](crate::Client::simple_query_compat) when
//! migrating code that consumes Tiberius `QueryStream` items. The bridge's
//! native buffered and row-only streaming APIs remain unchanged.

use std::borrow::Cow;
use std::fmt;
use std::future::poll_fn;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use futures_core::Stream;

use crate::row::RowSchema;
use crate::{Client, Column, ExecuteResult, Result, Row};

pub(crate) mod conversion;

pub use conversion::{ColumnData, FromSql, FromSqlOwned, IntoSql, ToSql};

/// A Tiberius-compatible dynamically bound query.
#[derive(Debug)]
pub struct Query<'a> {
    sql: Cow<'a, str>,
    params: Vec<ColumnData<'a>>,
}

impl<'a> Query<'a> {
    /// Create a query from borrowed or owned SQL.
    pub fn new(sql: impl Into<Cow<'a, str>>) -> Self {
        Self {
            sql: sql.into(),
            params: Vec::new(),
        }
    }

    /// Append a parameter in `@P1`, `@P2`, ... order.
    pub fn bind(&mut self, param: impl IntoSql<'a> + 'a) {
        self.params.push(param.into_sql());
    }

    /// Execute the query and return its compatibility item stream.
    pub async fn query<'b>(self, client: &'b mut Client) -> Result<QueryStream<'b>> {
        let params = self
            .params
            .iter()
            .map(|param| param as &dyn crate::query::ToSql)
            .collect::<Vec<_>>();
        let mut stream = client.query_compat(self.sql.into_owned(), &params);
        let _ = stream.columns().await?;
        Ok(stream)
    }

    /// Execute the query and return affected-row counts.
    pub async fn execute(self, client: &mut Client) -> Result<ExecuteResult> {
        let params = self
            .params
            .iter()
            .map(|param| param as &dyn crate::query::ToSql)
            .collect::<Vec<_>>();
        client.execute(self.sql.into_owned(), &params).await
    }
}

/// Metadata emitted before the rows of each result set.
#[derive(Debug, Clone)]
pub struct ResultMetadata {
    schema: Arc<RowSchema>,
    result_index: usize,
}

impl ResultMetadata {
    pub(crate) fn new(schema: Arc<RowSchema>, result_index: usize) -> Self {
        Self {
            schema,
            result_index,
        }
    }

    /// Columns in wire order for this result set.
    pub fn columns(&self) -> &[Column] {
        &self.schema.columns
    }

    /// Zero-based result-set index.
    pub fn result_index(&self) -> usize {
        self.result_index
    }
}

/// An item from a compatibility query stream.
#[derive(Debug)]
pub enum QueryItem {
    /// A row in the current result set.
    Row(Row),
    /// Metadata marking the start of a result set.
    Metadata(ResultMetadata),
}

impl QueryItem {
    /// Borrow this item as metadata, if it is metadata.
    pub fn as_metadata(&self) -> Option<&ResultMetadata> {
        match self {
            Self::Metadata(metadata) => Some(metadata),
            Self::Row(_) => None,
        }
    }

    /// Borrow this item as a row, if it is a row.
    pub fn as_row(&self) -> Option<&Row> {
        match self {
            Self::Row(row) => Some(row),
            Self::Metadata(_) => None,
        }
    }

    /// Consume this item as metadata, if it is metadata.
    pub fn into_metadata(self) -> Option<ResultMetadata> {
        match self {
            Self::Metadata(metadata) => Some(metadata),
            Self::Row(_) => None,
        }
    }

    /// Consume this item as a row, if it is a row.
    pub fn into_row(self) -> Option<Row> {
        match self {
            Self::Row(row) => Some(row),
            Self::Metadata(_) => None,
        }
    }
}

/// Incremental Tiberius-compatible metadata and row stream.
pub struct QueryStream<'a> {
    inner: Pin<Box<dyn Stream<Item = Result<QueryItem>> + Send + 'a>>,
    peeked: Option<Result<QueryItem>>,
    columns: Option<Arc<RowSchema>>,
}

impl fmt::Debug for QueryStream<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("QueryStream")
            .finish_non_exhaustive()
    }
}

impl<'a> QueryStream<'a> {
    pub(crate) fn new(inner: Pin<Box<dyn Stream<Item = Result<QueryItem>> + Send + 'a>>) -> Self {
        Self {
            inner,
            peeked: None,
            columns: None,
        }
    }

    /// Return columns for the current or next result set.
    ///
    /// A stream error encountered while looking ahead is returned once and
    /// consumed. Unlike Tiberius, the bridge error is not `Clone`, so a later
    /// [`Stream::poll_next`] observes the item after that error rather than
    /// receiving the same error again.
    pub async fn columns(&mut self) -> Result<Option<&[Column]>> {
        if self.peeked.is_none() {
            match poll_fn(|cx| self.inner.as_mut().poll_next(cx)).await {
                Some(Ok(item)) => {
                    if let QueryItem::Metadata(metadata) = &item {
                        self.columns = Some(Arc::clone(&metadata.schema));
                    }
                    self.peeked = Some(Ok(item));
                }
                Some(Err(error)) => {
                    self.columns = None;
                    return Err(error);
                }
                None => {}
            }
        }

        Ok(self
            .columns
            .as_deref()
            .map(|schema| schema.columns.as_slice()))
    }

    /// Flatten all result sets into a row-only stream.
    pub fn into_row_stream(mut self) -> Pin<Box<dyn Stream<Item = Result<Row>> + Send + 'a>> {
        Box::pin(async_stream::try_stream! {
            while let Some(item) = poll_fn(|cx| Pin::new(&mut self).poll_next(cx)).await {
                if let QueryItem::Row(row) = item? {
                    yield row;
                }
            }
        })
    }

    /// Collect all result sets.
    ///
    /// Unlike Tiberius at the pinned compatibility commit, the bridge keeps
    /// empty middle and trailing result sets instead of dropping them.
    pub async fn into_results(mut self) -> Result<Vec<Vec<Row>>> {
        let mut results = Vec::new();
        while let Some(item) = poll_fn(|cx| Pin::new(&mut self).poll_next(cx)).await {
            match item? {
                QueryItem::Metadata(_) => results.push(Vec::new()),
                QueryItem::Row(row) => {
                    if results.is_empty() {
                        results.push(Vec::new());
                    }
                    if let Some(result) = results.last_mut() {
                        result.push(row);
                    }
                }
            }
        }
        Ok(results)
    }

    /// Collect and return the first result set, draining later results.
    pub async fn into_first_result(self) -> Result<Vec<Row>> {
        Ok(self
            .into_results()
            .await?
            .into_iter()
            .next()
            .unwrap_or_default())
    }

    /// Collect and return the first row of the first result set.
    pub async fn into_row(self) -> Result<Option<Row>> {
        Ok(self.into_first_result().await?.into_iter().next())
    }
}

impl Stream for QueryStream<'_> {
    type Item = Result<QueryItem>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let item = match self.peeked.take() {
            Some(item) => Poll::Ready(Some(item)),
            None => self.inner.as_mut().poll_next(cx),
        };
        match &item {
            Poll::Ready(Some(Ok(QueryItem::Metadata(metadata)))) => {
                self.columns = Some(Arc::clone(&metadata.schema));
            }
            Poll::Ready(Some(Err(_))) => self.columns = None,
            _ => {}
        }
        item
    }
}

#[cfg(test)]
mod tests {
    use futures_util::{StreamExt, TryStreamExt};

    use super::*;
    use crate::Error;

    fn schema() -> Arc<RowSchema> {
        RowSchema::from_metadata(&[])
    }

    fn row() -> Row {
        Row::from_schema(schema(), Vec::new())
    }

    #[test]
    fn query_item_accessors_match_variants() {
        let metadata = QueryItem::Metadata(ResultMetadata::new(schema(), 3));
        assert_eq!(
            metadata
                .as_metadata()
                .expect("metadata variant")
                .result_index(),
            3
        );
        assert!(
            metadata
                .as_metadata()
                .expect("metadata variant")
                .columns()
                .is_empty()
        );
        assert!(metadata.as_row().is_none());
        assert_eq!(
            metadata
                .into_metadata()
                .expect("consume metadata variant")
                .result_index(),
            3
        );
        assert!(
            QueryItem::Metadata(ResultMetadata::new(schema(), 0))
                .into_row()
                .is_none()
        );

        let row_item = QueryItem::Row(row());
        assert!(row_item.as_metadata().is_none());
        assert!(row_item.as_row().is_some());
        assert!(row_item.into_row().is_some());
        assert!(QueryItem::Row(row()).into_metadata().is_none());
    }

    #[test]
    fn query_stream_debug_omits_inner_stream() {
        let stream = QueryStream::new(Box::pin(futures_util::stream::empty()));
        assert_eq!(format!("{stream:?}"), "QueryStream { .. }");
    }

    #[tokio::test]
    async fn columns_handles_empty_row_and_error_streams() {
        let mut empty = QueryStream::new(Box::pin(futures_util::stream::empty()));
        assert!(
            empty
                .columns()
                .await
                .expect("empty stream columns")
                .is_none()
        );
        assert!(
            empty
                .columns()
                .await
                .expect("repeated empty columns")
                .is_none()
        );

        let mut row_first = QueryStream::new(Box::pin(futures_util::stream::iter([Ok(
            QueryItem::Row(row()),
        )])));
        assert!(
            row_first
                .columns()
                .await
                .expect("row-only stream columns")
                .is_none()
        );
        assert!(matches!(
            row_first.next().await,
            Some(Ok(QueryItem::Row(_)))
        ));

        let mut error = QueryStream::new(Box::pin(futures_util::stream::iter([Err(
            Error::Conversion("expected".into()),
        )])));
        assert!(matches!(
            error.columns().await,
            Err(Error::Conversion(message)) if message == "expected"
        ));
        assert!(error.next().await.is_none());
        assert!(
            error
                .columns()
                .await
                .expect("columns after error")
                .is_none()
        );
    }

    #[tokio::test]
    async fn direct_stream_error_clears_columns() {
        let mut stream = QueryStream::new(Box::pin(futures_util::stream::iter([
            Ok(QueryItem::Metadata(ResultMetadata::new(schema(), 0))),
            Err(Error::Conversion("expected".into())),
        ])));

        assert!(stream.columns().await.expect("columns").is_some());
        assert!(stream.next().await.is_some());
        assert!(matches!(
            stream.next().await,
            Some(Err(Error::Conversion(message))) if message == "expected"
        ));
        assert!(
            stream
                .columns()
                .await
                .expect("columns after error")
                .is_none()
        );
    }

    #[tokio::test]
    async fn clean_eof_keeps_current_columns() {
        let mut stream = QueryStream::new(Box::pin(futures_util::stream::iter([Ok(
            QueryItem::Metadata(ResultMetadata::new(schema(), 0)),
        )])));

        assert!(stream.columns().await.expect("columns").is_some());
        assert!(stream.next().await.is_some());
        assert!(stream.next().await.is_none());
        assert!(
            stream
                .columns()
                .await
                .expect("columns after clean EOF")
                .is_some()
        );
    }

    #[tokio::test]
    async fn columns_peeks_each_result_boundary() {
        let first = ResultMetadata::new(schema(), 0);
        let second = ResultMetadata::new(schema(), 1);
        let mut stream = QueryStream::new(Box::pin(futures_util::stream::iter([
            Ok(QueryItem::Metadata(first)),
            Ok(QueryItem::Row(row())),
            Ok(QueryItem::Metadata(second)),
        ])));

        assert!(stream.columns().await.expect("first columns").is_some());
        assert!(matches!(
            stream.next().await,
            Some(Ok(QueryItem::Metadata(metadata))) if metadata.result_index() == 0
        ));
        assert!(stream.columns().await.expect("row columns").is_some());
        assert!(matches!(stream.next().await, Some(Ok(QueryItem::Row(_)))));
        assert!(stream.columns().await.expect("second columns").is_some());
        assert!(matches!(
            stream.next().await,
            Some(Ok(QueryItem::Metadata(metadata))) if metadata.result_index() == 1
        ));
    }

    #[tokio::test]
    async fn collectors_cover_empty_error_and_row_only_inputs() {
        let row_only = QueryStream::new(Box::pin(futures_util::stream::iter([Ok(
            QueryItem::Row(row()),
        )])));
        let results = row_only
            .into_results()
            .await
            .expect("collect row-only input");
        assert_eq!(results.len(), 1);
        assert_eq!(results.first().map(Vec::len), Some(1));

        let empty = QueryStream::new(Box::pin(futures_util::stream::empty()));
        assert!(
            empty
                .into_first_result()
                .await
                .expect("collect empty first result")
                .is_empty()
        );
        let empty = QueryStream::new(Box::pin(futures_util::stream::empty()));
        assert!(empty.into_row().await.expect("collect empty row").is_none());

        let error = QueryStream::new(Box::pin(futures_util::stream::iter([Err(
            Error::Conversion("expected".into()),
        )])));
        assert!(matches!(
            error.into_row_stream().try_collect::<Vec<_>>().await,
            Err(Error::Conversion(message)) if message == "expected"
        ));
    }
}
