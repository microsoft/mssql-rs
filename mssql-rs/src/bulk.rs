// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

//! Bulk insert (BCP) — high-throughput row loading via the TDS BulkLoadBCP token.
//!
//! Mirrors tiberius' `Client::bulk_insert` API on top of mssql-tds'
//! [`mssql_tds::connection::bulk_copy::BulkCopy`] surface. Bulk insert streams
//! rows in their TDS wire format.
//!
//! # Quick start
//!
//! ```rust,no_run
//! use std::time::Duration;
//! use async_trait::async_trait;
//! use mssql_rs::{Client, Config, AuthMethod, Result};
//! use mssql_rs::bulk::{BulkLoadRow, ColumnMapping};
//! use mssql_tds::core::TdsResult;
//! use mssql_tds::datatypes::column_values::ColumnValues;
//! use mssql_tds::datatypes::sql_string::SqlString;
//! use mssql_tds::message::bulk_load::StreamingBulkLoadWriter;
//!
//! struct User { id: i32, name: String }
//!
//! #[async_trait]
//! impl BulkLoadRow for User {
//!     async fn write_to_packet(
//!         &self,
//!         writer: &mut StreamingBulkLoadWriter<'_>,
//!         column_index: &mut usize,
//!     ) -> TdsResult<()> {
//!         writer.write_column_value(*column_index, &ColumnValues::Int(self.id)).await?;
//!         *column_index += 1;
//!         writer.write_column_value(
//!             *column_index,
//!             &ColumnValues::String(SqlString::from_utf8_string(self.name.clone())),
//!         ).await?;
//!         *column_index += 1;
//!         Ok(())
//!     }
//! }
//!
//! # async fn run(client: &mut Client) -> Result<()> {
//! let users = vec![
//!     User { id: 1, name: "Ada".into() },
//!     User { id: 2, name: "Grace".into() },
//! ];
//! let result = client
//!     .bulk_insert("Users")
//!     .batch_size(5000)
//!     .timeout(Duration::from_secs(60))
//!     .table_lock(true)
//!     .send(users)
//!     .await?;
//! println!("loaded {} rows in {:?}", result.rows_affected, result.elapsed);
//! # Ok(()) }
//! ```
//!
//! # Column mapping
//!
//! By default, source row columns map to the destination table by ordinal
//! (skipping identity columns unless [`BulkInsert::keep_identity`] is set).
//! Explicit ordinal mappings must follow destination column order, with source
//! ordinals starting at zero and no gaps. The streaming row interface cannot
//! reorder or skip source values. Name-based source mappings are unsupported.
//!
//! # Options
//!
//! All `SqlBulkCopyOptions`-equivalent flags are exposed as builder methods:
//! [`keep_identity`](BulkInsert::keep_identity),
//! [`keep_nulls`](BulkInsert::keep_nulls),
//! [`table_lock`](BulkInsert::table_lock),
//! [`check_constraints`](BulkInsert::check_constraints),
//! [`fire_triggers`](BulkInsert::fire_triggers),
//! [`use_internal_transaction`](BulkInsert::use_internal_transaction),
//! [`batch_size`](BulkInsert::batch_size),
//! [`timeout`](BulkInsert::timeout),
//! [`notification_interval`](BulkInsert::notification_interval).

use std::future::{IntoFuture, Ready, ready};
use std::time::Duration;

use async_trait::async_trait;
use mssql_tds::connection::bulk_copy::BulkCopy as TdsBulkCopy;
use mssql_tds::connection::tds_client::TdsClient;
use mssql_tds::core::TdsResult;
use mssql_tds::message::bulk_load::StreamingBulkLoadWriter;

use crate::compat::ColumnData;
use crate::error::{Error, Result};
use crate::operation::Operation;
use crate::{ExecuteResult, IntoSql};

// Re-export upstream types that callers will use directly.
pub use mssql_tds::connection::bulk_copy::{
    BulkCopyOptions, BulkCopyProgress, BulkCopyResult, BulkLoadRow, ColumnMapping,
    ColumnMappingSource,
};

/// Builder + executor for a single bulk insert into one destination table.
///
/// Created via [`Client::bulk_insert`](crate::Client::bulk_insert) or
/// [`Client::bulk_insert_with_columns`](crate::Client::bulk_insert_with_columns).
/// Configure it with the option setters, then call [`send`](Self::send) to
/// stream rows.
///
/// Dropping a pending send marks the native connection dead; dropping an unused
/// builder or unpolled send does not. See [`Client`](crate::Client)'s
/// cancellation safety contract.
pub struct BulkInsert<'a> {
    client: &'a mut TdsClient,
    table_name: String,
    options: BulkCopyOptions,
    timeout: Option<Duration>,
    column_mappings: Vec<ColumnMapping>,
}

/// One row for the Tiberius-compatible incremental bulk API.
#[derive(Debug, Default, Clone)]
pub struct TokenRow<'a> {
    data: Vec<ColumnData<'a>>,
}

impl<'a> TokenRow<'a> {
    /// Create an empty row.
    pub const fn new() -> Self {
        Self { data: Vec::new() }
    }

    /// Create an empty row with allocated capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            data: Vec::with_capacity(capacity),
        }
    }

    /// Remove all values without changing the allocated capacity.
    pub fn clear(&mut self) {
        self.data.clear();
    }

    /// Return the number of values.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Return whether the row contains no values.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Borrow the value at `index`.
    pub fn get(&self, index: usize) -> Option<&ColumnData<'a>> {
        self.data.get(index)
    }

    /// Iterate over values in column order.
    pub fn iter(&self) -> std::slice::Iter<'_, ColumnData<'a>> {
        self.data.iter()
    }

    /// Append one value.
    pub fn push(&mut self, value: ColumnData<'a>) {
        self.data.push(value);
    }
}

impl<'a> IntoIterator for TokenRow<'a> {
    type Item = ColumnData<'a>;
    type IntoIter = std::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.data.into_iter()
    }
}

#[async_trait]
impl BulkLoadRow for TokenRow<'_> {
    async fn write_to_packet(
        &self,
        writer: &mut StreamingBulkLoadWriter<'_>,
        column_index: &mut usize,
    ) -> TdsResult<()> {
        for value in &self.data {
            let value = value
                .clone()
                .into_column_value()
                .map_err(|error| mssql_tds::error::Error::UsageError(error.to_string()))?;
            writer.write_column_value(*column_index, &value).await?;
            *column_index += 1;
        }
        Ok(())
    }
}

/// Convert a scalar or tuple into a compatibility bulk row.
pub trait IntoRow<'a> {
    /// Convert values to a row in tuple order, or return a conversion error.
    fn into_row(self) -> Result<TokenRow<'a>>;
}

impl<'a, A> IntoRow<'a> for A
where
    A: IntoSql<'a>,
{
    fn into_row(self) -> Result<TokenRow<'a>> {
        let mut row = TokenRow::with_capacity(1);
        row.push(self.into_sql()?);
        Ok(row)
    }
}

macro_rules! impl_into_row {
    ($len:expr; $( $type:ident $field:tt ),+) => {
        impl<'a, $( $type ),+> IntoRow<'a> for ($( $type, )+)
        where
            $( $type: IntoSql<'a>, )+
        {
            fn into_row(self) -> Result<TokenRow<'a>> {
                let mut row = TokenRow::with_capacity($len);
                $( row.push(self.$field.into_sql()?); )+
                Ok(row)
            }
        }
    };
}

impl_into_row!(2; A 0, B 1);
impl_into_row!(3; A 0, B 1, C 2);
impl_into_row!(4; A 0, B 1, C 2, D 3);
impl_into_row!(5; A 0, B 1, C 2, D 3, E 4);
impl_into_row!(6; A 0, B 1, C 2, D 3, E 4, F 5);
impl_into_row!(7; A 0, B 1, C 2, D 3, E 4, F 5, G 6);
impl_into_row!(8; A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7);
impl_into_row!(9; A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8);
impl_into_row!(10; A 0, B 1, C 2, D 3, E 4, F 5, G 6, H 7, I 8, J 9);

/// Tiberius-compatible incremental bulk request.
///
/// Rows are retained until [`finalize`](Self::finalize), then sent through one
/// native [`BulkInsert`] operation. Dropping the request before finalization
/// sends nothing and leaves the connection usable.
pub struct BulkLoadRequest<'a> {
    bulk: BulkInsert<'a>,
    rows: Vec<TokenRow<'a>>,
    row_width: Option<usize>,
}

impl<'a> BulkLoadRequest<'a> {
    /// Add one row to this request.
    ///
    /// A width mismatch with an earlier row is rejected without changing the
    /// request, so a corrected row can still be sent.
    pub async fn send(&mut self, row: TokenRow<'a>) -> Result<()> {
        validate_row_width(&mut self.row_width, row.len())?;
        self.rows.push(row);
        Ok(())
    }

    /// Send all retained rows and finish the bulk operation.
    pub async fn finalize(self) -> Result<ExecuteResult> {
        let result = self
            .bulk
            .send(self.rows)
            .await
            .map_err(map_compat_bulk_error)?;
        Ok(ExecuteResult {
            counts: vec![result.rows_affected],
        })
    }
}

fn validate_row_width(expected: &mut Option<usize>, actual: usize) -> Result<()> {
    match *expected {
        Some(expected) if actual != expected => Err(Error::BulkInput(format!(
            "Expecting {expected} columns but {actual} were given"
        ))),
        Some(_) => Ok(()),
        None => {
            *expected = Some(actual);
            Ok(())
        }
    }
}

fn map_compat_bulk_error(error: Error) -> Error {
    match error {
        Error::Tds(mssql_tds::error::Error::UsageError(message))
            if is_bulk_input_message(&message) =>
        {
            Error::BulkInput(message)
        }
        error => error,
    }
}

fn is_bulk_input_message(message: &str) -> bool {
    [
        "Row ",
        "Column index ",
        "Binary data length ",
        "String length ",
        "SQL_VARIANT data size ",
        "SQL_VARIANT total size ",
        "Cannot serialize NULL ",
        "Unsupported TDS type ",
        "Invalid UTF-16 data: ",
        "Conversion error: ",
    ]
    .iter()
    .any(|prefix| message.starts_with(prefix))
}

impl<'a> IntoFuture for BulkInsert<'a> {
    type Output = Result<BulkLoadRequest<'a>>;
    type IntoFuture = Ready<Self::Output>;

    fn into_future(self) -> Self::IntoFuture {
        ready(
            crate::operation::ensure_usable(self.client).map(|()| BulkLoadRequest {
                bulk: self,
                rows: Vec::new(),
                row_width: None,
            }),
        )
    }
}

impl<'a> BulkInsert<'a> {
    /// Construct a `BulkInsert` for the given destination table.
    ///
    /// Prefer [`Client::bulk_insert`](crate::Client::bulk_insert) — this is the
    /// low-level entry point for callers that already hold a
    /// [`TdsClient`] reference
    /// (e.g., via [`Client::inner_mut`](crate::Client::inner_mut)).
    pub fn new(client: &'a mut TdsClient, table_name: impl Into<String>) -> Self {
        Self {
            client,
            table_name: table_name.into(),
            options: BulkCopyOptions::default(),
            timeout: None,
            column_mappings: Vec::new(),
        }
    }

    /// Number of rows per server-side batch. Default 0 = single batch.
    pub fn batch_size(mut self, n: usize) -> Self {
        self.options.batch_size = n;
        self
    }

    /// Per-operation timeout. Default 30 seconds. Pass `Duration::ZERO` for no timeout.
    pub fn timeout(mut self, t: Duration) -> Self {
        self.timeout = Some(t);
        self
    }

    /// Enforce CHECK constraints on the destination table during the load. Default off.
    pub fn check_constraints(mut self, enabled: bool) -> Self {
        self.options.check_constraints = enabled;
        self
    }

    /// Fire INSERT triggers for every loaded row. Default off.
    pub fn fire_triggers(mut self, enabled: bool) -> Self {
        self.options.fire_triggers = enabled;
        self
    }

    /// Preserve source identity column values. Default off (server auto-generates).
    pub fn keep_identity(mut self, enabled: bool) -> Self {
        self.options.keep_identity = enabled;
        self
    }

    /// Preserve source NULLs even when destination has a DEFAULT. Default off.
    pub fn keep_nulls(mut self, enabled: bool) -> Self {
        self.options.keep_nulls = enabled;
        self
    }

    /// Acquire a bulk-update (TABLOCK) lock for the duration of the load. Default off.
    pub fn table_lock(mut self, enabled: bool) -> Self {
        self.options.table_lock = enabled;
        self
    }

    /// Wrap each batch in its own server-side transaction. Default off.
    ///
    /// **Cannot be combined with an active client-level transaction.**
    pub fn use_internal_transaction(mut self, enabled: bool) -> Self {
        self.options.use_internal_transaction = enabled;
        self
    }

    /// Rows between progress callback invocations. Default 0 = no callbacks.
    pub fn notification_interval(mut self, n: usize) -> Self {
        self.options.notification_interval = n;
        self
    }

    /// Add an explicit source → destination column mapping.
    ///
    /// When any mapping is added, ordinal auto-mapping is disabled and only
    /// the listed mappings apply. In destination order, source ordinals must
    /// start at zero and be consecutive; streaming rows cannot be reordered.
    /// Name-based source mappings are unsupported.
    pub fn add_column_mapping(mut self, mapping: ColumnMapping) -> Self {
        self.column_mappings.push(mapping);
        self
    }

    /// Create a name-based mapping. Streaming rows do not expose source names,
    /// so sending with this mapping returns an error.
    pub fn map_column(self, source_name: impl Into<String>, dest_name: impl Into<String>) -> Self {
        self.add_column_mapping(ColumnMapping::by_name(source_name, dest_name))
    }

    /// Map source ordinal `source_ord` (0-based) to destination column `dest_name`.
    /// Source ordinals must be consecutive in destination order.
    pub fn map_column_by_ordinal(self, source_ord: usize, dest_name: impl Into<String>) -> Self {
        self.add_column_mapping(ColumnMapping::by_ordinal(source_ord, dest_name))
    }

    /// Stream the row iterator to the server.
    ///
    /// Each `R: BulkLoadRow` writes its columns directly into the streaming
    /// TDS packet — no intermediate buffering, no allocation per row beyond
    /// what the row itself owns.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Tds`] for any failure: column type mismatch, network
    /// error, server-side constraint violation, timeout, or a known-dead client.
    /// Completed errors retain the native driver's recovery/liveness outcome.
    /// Dropping this future during I/O bypasses native async cleanup and marks
    /// the connection dead; discard it rather than retrying on the same client.
    pub async fn send<I, R>(self, rows: I) -> Result<BulkCopyResult>
    where
        I: IntoIterator<Item = R>,
        R: BulkLoadRow,
    {
        let mut operation = Operation::new(self.client)?;
        let result = async {
            let mut bulk = TdsBulkCopy::new(&mut operation, self.table_name)
                .batch_size(self.options.batch_size)
                .check_constraints(self.options.check_constraints)
                .fire_triggers(self.options.fire_triggers)
                .keep_identity(self.options.keep_identity)
                .keep_nulls(self.options.keep_nulls)
                .table_lock(self.options.table_lock)
                .use_internal_transaction(self.options.use_internal_transaction)
                .notification_interval(self.options.notification_interval);
            if let Some(timeout) = self.timeout {
                bulk = bulk.timeout(timeout);
            }
            let has_mappings = !self.column_mappings.is_empty();
            for mapping in self.column_mappings {
                bulk = bulk.add_column_mapping(mapping);
            }
            if has_mappings {
                let mappings = bulk.get_resolved_mappings().await.map_err(Error::Tds)?;
                if mappings
                    .iter()
                    .enumerate()
                    .any(|(index, mapping)| mapping.source_index != index)
                {
                    return Err(Error::Tds(mssql_tds::error::Error::UsageError(
                        "Bulk source ordinals must start at zero and be consecutive in destination column order; streaming rows cannot be reordered or skip values".to_string(),
                    )));
                }
            }
            bulk.write_to_server_zerocopy(rows)
                .await
                .map_err(Error::Tds)
        }
        .await;
        operation.complete(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // No DB-required tests live here; the BulkCopy state is internal to
    // the upstream crate, so we can't construct a BulkInsert<'a> without
    // a TdsClient. We just sanity-check that the re-exported types resolve
    // and that the convenience mapping helpers produce the expected variants.

    #[test]
    fn map_column_by_name_creates_named_mapping() {
        let m = ColumnMapping::by_name("src", "dst");
        assert!(matches!(m.source, ColumnMappingSource::Name(n) if n == "src"));
        assert_eq!(m.destination, "dst");
    }

    #[test]
    fn map_column_by_ordinal_creates_ordinal_mapping() {
        let m = ColumnMapping::by_ordinal(3, "dst");
        assert!(matches!(m.source, ColumnMappingSource::Ordinal(3)));
        assert_eq!(m.destination, "dst");
    }

    #[test]
    fn bulk_copy_options_defaults_match_dotnet_sqlbulkcopy() {
        let o = BulkCopyOptions::default();
        assert_eq!(o.batch_size, 0);
        assert_eq!(o.timeout_sec, 30);
        assert!(!o.check_constraints);
        assert!(!o.fire_triggers);
        assert!(!o.keep_identity);
        assert!(!o.keep_nulls);
        assert!(!o.table_lock);
        assert!(!o.use_internal_transaction);
    }

    #[test]
    fn bulk_copy_result_computes_throughput() {
        let r = BulkCopyResult::new(10_000, Duration::from_secs(2));
        assert_eq!(r.rows_affected, 10_000);
        assert_eq!(r.elapsed, Duration::from_secs(2));
        assert!((r.rows_per_second - 5_000.0).abs() < 1.0);
    }

    #[test]
    fn bulk_copy_result_zero_elapsed_yields_zero_throughput() {
        let r = BulkCopyResult::new(100, Duration::ZERO);
        assert_eq!(r.rows_per_second.to_bits(), 0.0_f64.to_bits());
    }

    #[test]
    fn token_row_helpers_and_tuple_arities_preserve_values() {
        assert!(TokenRow::new().is_empty());
        let mut row = TokenRow::with_capacity(2);
        assert!(row.is_empty());
        row.push(1i32.into_sql().expect("valid parameter"));
        row.push(Option::<&str>::None.into_sql().expect("valid parameter"));
        assert_eq!(row.len(), 2);
        assert!(matches!(row.get(0), Some(ColumnData::I32(Some(1)))));
        assert_eq!(row.iter().count(), 2);
        assert_eq!(row.clone().into_iter().count(), 2);
        row.clear();
        assert!(row.is_empty());

        assert_eq!(1i32.into_row().expect("valid row").len(), 1);
        assert_eq!((1i32, 2i32).into_row().expect("valid row").len(), 2);
        assert_eq!((1i32, 2i32, 3i32).into_row().expect("valid row").len(), 3);
        assert_eq!(
            (1i32, 2i32, 3i32, 4i32)
                .into_row()
                .expect("valid row")
                .len(),
            4
        );
        assert_eq!(
            (1i32, 2i32, 3i32, 4i32, 5i32)
                .into_row()
                .expect("valid row")
                .len(),
            5
        );
        assert_eq!(
            (1i32, 2i32, 3i32, 4i32, 5i32, 6i32)
                .into_row()
                .expect("valid row")
                .len(),
            6
        );
        assert_eq!(
            (1i32, 2i32, 3i32, 4i32, 5i32, 6i32, 7i32)
                .into_row()
                .expect("valid row")
                .len(),
            7
        );
        assert_eq!(
            (1i32, 2i32, 3i32, 4i32, 5i32, 6i32, 7i32, 8i32)
                .into_row()
                .expect("valid row")
                .len(),
            8
        );
        assert_eq!(
            (1i32, 2i32, 3i32, 4i32, 5i32, 6i32, 7i32, 8i32, 9i32)
                .into_row()
                .expect("valid row")
                .len(),
            9
        );
        assert_eq!(
            (1i32, 2i32, 3i32, 4i32, 5i32, 6i32, 7i32, 8i32, 9i32, 10i32)
                .into_row()
                .expect("valid row")
                .len(),
            10
        );
    }

    #[test]
    fn compatibility_usage_errors_are_bulk_input_errors() {
        let error = map_compat_bulk_error(Error::Tds(mssql_tds::error::Error::UsageError(
            "Row 1 wrote 1 columns, but expected 2 columns based on table metadata".into(),
        )));
        assert!(matches!(error, Error::BulkInput(message) if message.starts_with("Row 1")));

        let error = map_compat_bulk_error(Error::Tds(mssql_tds::error::Error::UsageError(
            "Column index 1 out of bounds, expected 1 columns based on table metadata.".into(),
        )));
        assert!(
            matches!(error, Error::BulkInput(message) if message.starts_with("Column index 1"))
        );

        let error = map_compat_bulk_error(Error::Conversion("not bulk".into()));
        assert!(matches!(error, Error::Conversion(message) if message == "not bulk"));

        let error = map_compat_bulk_error(Error::Tds(mssql_tds::error::Error::UsageError(
            "Table not found or has no columns".into(),
        )));
        assert!(matches!(
            error,
            Error::Tds(mssql_tds::error::Error::UsageError(message))
                if message == "Table not found or has no columns"
        ));
    }

    #[test]
    fn row_width_failure_does_not_change_expected_width() {
        let mut expected = None;
        validate_row_width(&mut expected, 2).expect("first row sets width");
        assert!(matches!(
            validate_row_width(&mut expected, 1),
            Err(Error::BulkInput(_))
        ));
        assert_eq!(expected, Some(2));
        validate_row_width(&mut expected, 2).expect("corrected row remains valid");
    }
}
