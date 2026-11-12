// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

//! Query result types and the [`ToSql`] trait for parameter binding.
//!
//! [`QueryResult`] wraps streamed result sets from SQL Server and provides
//! [`into_first_result()`](QueryResult::into_first_result) (single result set)
//! and [`into_results()`](QueryResult::into_results) (multiple result sets).

use mssql_tds::datatypes::sql_string::SqlString;
use mssql_tds::datatypes::sqltypes::SqlType;
use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};

use crate::row::Row;

pub use crate::compat::Query;

/// Result of an `execute()` call, containing row counts per statement.
#[derive(Debug, Clone)]
pub struct ExecuteResult {
    pub(crate) counts: Vec<u64>,
}

impl ExecuteResult {
    /// Per-statement row counts in statement order.
    pub fn rows_affected(&self) -> &[u64] {
        self.counts.as_slice()
    }

    /// Total rows affected across all statements.
    pub fn total(&self) -> u64 {
        self.counts.iter().sum()
    }

    /// Iterate over per-statement row counts.
    #[expect(
        clippy::should_implement_trait,
        reason = "The existing inherent method must remain source-compatible alongside IntoIterator"
    )]
    pub fn into_iter(self) -> impl Iterator<Item = u64> {
        self.counts.into_iter()
    }
}

impl IntoIterator for ExecuteResult {
    type Item = u64;
    type IntoIter = std::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        self.counts.into_iter()
    }
}

/// Collected query results from one or more SQL statements.
///
/// Use [`into_first_result()`](Self::into_first_result) for single-statement
/// queries (most common), or [`into_results()`](Self::into_results) for
/// multi-statement batches.
pub struct QueryResult {
    pub(crate) result_sets: Vec<Vec<Row>>,
}

impl QueryResult {
    /// Consume the first result set into a `Vec<Row>`.
    ///
    /// This is the most common access pattern, equivalent to tiberius'
    /// `stream.into_first_result().await?`.
    ///
    /// Returns an empty `Vec` if the query produced no result set.
    pub fn into_first_result(self) -> Vec<Row> {
        let mut sets = self.result_sets;
        if sets.is_empty() {
            return Vec::new();
        }
        sets.remove(0)
    }

    /// Consume all result sets into a `Vec<Vec<Row>>`.
    ///
    /// Use for multi-statement batches like `SELECT 1; SELECT 2`.
    pub fn into_results(self) -> Vec<Vec<Row>> {
        self.result_sets
    }

    /// Number of result sets.
    pub fn result_set_count(&self) -> usize {
        self.result_sets.len()
    }

    /// Consume into a [`Stream`](futures_core::Stream) of rows across all
    /// result sets.
    ///
    /// Mirrors tiberius' `QueryStream::into_row_stream()` for API
    /// compatibility. Use this when migrating code that calls
    /// `.into_row_stream().map(...).next().await` (e.g., the
    /// `windmill-worker` MSSQL S3 export path).
    ///
    /// # Limitations
    ///
    /// **Rows are pre-collected.** Unlike tiberius (which streams from the
    /// wire), this yields rows that have already been buffered into memory
    /// during the originating `query()` / `simple_query()` call. The
    /// streaming API is preserved for migration ergonomics, but wire-level
    /// streaming will require the `Client::query_streamed` follow-up
    /// tracked in <https://github.com/saurabh500/mssql-tiberius-bridge/issues/20>.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use mssql_rs::Client;
    /// use futures_util::StreamExt;
    ///
    /// # async fn example(client: &mut Client) -> mssql_rs::Result<()> {
    /// let mut stream = client
    ///     .simple_query("SELECT 1 AS n UNION ALL SELECT 2")
    ///     .await?
    ///     .into_row_stream();
    /// while let Some(row) = stream.next().await {
    ///     let row = row?;
    ///     println!("{:?}", row.get::<i32, _>("n"));
    /// }
    /// # Ok(())
    /// # }
    /// ```
    pub fn into_row_stream(self) -> RowStream {
        let total: usize = self.result_sets.iter().map(|s| s.len()).sum();
        let mut sets = self.result_sets.into_iter();
        let current = sets.next().unwrap_or_default().into_iter();
        RowStream {
            sets,
            current,
            remaining: total,
        }
    }

    /// Create an empty QueryResult.
    #[cfg(test)]
    pub(crate) fn empty() -> Self {
        QueryResult {
            result_sets: Vec::new(),
        }
    }
}

/// A `Stream` of [`Row`]s yielded across all result sets of a buffered
/// [`QueryResult`].
///
/// Created by [`QueryResult::into_row_stream`]. Implements
/// [`futures_core::Stream`] so it composes with `StreamExt`/`TryStreamExt`
/// (`.map`, `.try_next`, `.collect`, etc.) — matching the API surface
/// callers used with tiberius' `into_row_stream()`.
///
/// **Note:** rows are pre-buffered (see
/// [`QueryResult::into_row_stream`] for the limitation and roadmap).
pub struct RowStream {
    sets: std::vec::IntoIter<Vec<Row>>,
    current: std::vec::IntoIter<Row>,
    remaining: usize,
}

impl futures_core::Stream for RowStream {
    type Item = crate::error::Result<Row>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        loop {
            if let Some(row) = self.current.next() {
                self.remaining -= 1;
                return std::task::Poll::Ready(Some(Ok(row)));
            }
            match self.sets.next() {
                Some(next_set) => {
                    self.current = next_set.into_iter();
                }
                None => return std::task::Poll::Ready(None),
            }
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (self.remaining, Some(self.remaining))
    }
}

// ---------------------------------------------------------------------------
// ToSql — convert Rust types to RPC parameters
// ---------------------------------------------------------------------------

/// Trait for types that can be used as query parameters.
///
/// Implemented for common Rust types:
///
/// | Rust type | SQL Server type |
/// |-----------|------------------|
/// | `bool` | `bit` |
/// | `u8` | `tinyint` |
/// | `i16` | `smallint` |
/// | `i32` | `int` |
/// | `i64` | `bigint` |
/// | `f32` | `real` |
/// | `f64` | `float` |
/// | `&str` | `nvarchar(4000)` or `nvarchar(max)` |
/// | `String` | `nvarchar(4000)` or `nvarchar(max)` |
/// | `uuid::Uuid` | `uniqueidentifier` |
/// | `Option<T>` | Nullable version of inner type |
pub trait ToSql: Send + Sync {
    /// Convert this value into an mssql-tds `SqlType` for parameter binding.
    ///
    /// # Panics
    ///
    /// Date-bearing temporal implementations panic if the stored date is outside
    /// SQL Server's range (0001-01-01 through 9999-12-31). For offset datetimes,
    /// this range applies to the UTC date.
    fn to_sql(&self) -> SqlType;

    fn debug_fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "<sql param>")
    }
}

impl std::fmt::Debug for dyn ToSql + '_ {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.debug_fmt(f)
    }
}

pub struct DebugParams<'a>(pub &'a [&'a dyn ToSql]);

impl std::fmt::Debug for DebugParams<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|p| *p as &dyn ToSql))
            .finish()
    }
}

impl ToSql for bool {
    fn to_sql(&self) -> SqlType {
        SqlType::Bit(Some(*self))
    }

    fn debug_fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

impl ToSql for u8 {
    fn to_sql(&self) -> SqlType {
        SqlType::TinyInt(Some(*self))
    }

    fn debug_fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

impl ToSql for i16 {
    fn to_sql(&self) -> SqlType {
        SqlType::SmallInt(Some(*self))
    }

    fn debug_fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

impl ToSql for i32 {
    fn to_sql(&self) -> SqlType {
        SqlType::Int(Some(*self))
    }

    fn debug_fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

impl ToSql for i64 {
    fn to_sql(&self) -> SqlType {
        SqlType::BigInt(Some(*self))
    }

    fn debug_fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

impl ToSql for f32 {
    fn to_sql(&self) -> SqlType {
        SqlType::Real(Some(*self))
    }

    fn debug_fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

impl ToSql for f64 {
    fn to_sql(&self) -> SqlType {
        SqlType::Float(Some(*self))
    }

    fn debug_fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

pub(crate) fn nvarchar_from_string(value: Option<String>) -> SqlType {
    match value {
        Some(value) if value.encode_utf16().count() > 4000 => {
            SqlType::NVarcharMax(Some(SqlString::from_utf8_string(value)))
        }
        value => SqlType::NVarchar(value.map(SqlString::from_utf8_string), 4000),
    }
}

impl ToSql for &str {
    fn to_sql(&self) -> SqlType {
        nvarchar_from_string(Some(self.to_string()))
    }

    fn debug_fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

impl ToSql for String {
    fn to_sql(&self) -> SqlType {
        nvarchar_from_string(Some(self.clone()))
    }

    fn debug_fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

impl ToSql for uuid::Uuid {
    fn to_sql(&self) -> SqlType {
        SqlType::Uuid(Some(*self))
    }
}

// ---------------------------------------------------------------------------
// rust_decimal
// ---------------------------------------------------------------------------

/// Converts a decimal string to `SqlType::Numeric` with the given precision/scale.
/// Falls back to max precision (38) if the initial precision is insufficient, or
/// returns `SqlType::Numeric(None)` if all attempts fail.
fn decimal_to_sql_type(decimal_str: &str, precision: u8, scale: u8) -> SqlType {
    use mssql_tds::datatypes::decoder::DecimalParts;

    match DecimalParts::from_string(decimal_str, precision, scale) {
        Ok(dp) => SqlType::Numeric(Some(dp)),
        Err(_) => DecimalParts::from_string(decimal_str, 38, scale)
            .map(|dp| SqlType::Numeric(Some(dp)))
            .unwrap_or_else(|_| SqlType::Numeric(None)),
    }
}

impl ToSql for rust_decimal::Decimal {
    fn to_sql(&self) -> SqlType {
        let decimal_str = self.to_string();
        let scale = self.scale() as u8;

        let digits = self.mantissa().unsigned_abs().checked_ilog10().unwrap_or(0) + 1;
        let precision = u8::try_from(digits).expect("a decimal mantissa has at most 29 digits");

        let precision = precision.max(scale);

        decimal_to_sql_type(&decimal_str, precision, scale)
    }

    fn debug_fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

fn into_typed_null(value: SqlType) -> SqlType {
    match value {
        SqlType::Bit(_) => SqlType::Bit(None),
        SqlType::TinyInt(_) => SqlType::TinyInt(None),
        SqlType::SmallInt(_) => SqlType::SmallInt(None),
        SqlType::Int(_) => SqlType::Int(None),
        SqlType::BigInt(_) => SqlType::BigInt(None),
        SqlType::Real(_) => SqlType::Real(None),
        SqlType::Float(_) => SqlType::Float(None),
        SqlType::Decimal(_) => SqlType::Decimal(None),
        SqlType::Numeric(_) => SqlType::Numeric(None),
        SqlType::Money(_) => SqlType::Money(None),
        SqlType::SmallMoney(_) => SqlType::SmallMoney(None),
        SqlType::Time(_) => SqlType::Time(None),
        SqlType::DateTime2(_) => SqlType::DateTime2(None),
        SqlType::DateTimeOffset(_) => SqlType::DateTimeOffset(None),
        SqlType::SmallDateTime(_) => SqlType::SmallDateTime(None),
        SqlType::DateTime(_) => SqlType::DateTime(None),
        SqlType::Date(_) => SqlType::Date(None),
        SqlType::NVarchar(_, length) => SqlType::NVarchar(None, length),
        SqlType::NVarcharMax(_) => SqlType::NVarcharMax(None),
        SqlType::Varchar(_, length) => SqlType::Varchar(None, length),
        SqlType::VarcharMax(_) => SqlType::VarcharMax(None),
        SqlType::VarBinary(_, length) => SqlType::VarBinary(None, length),
        SqlType::VarBinaryMax(_) => SqlType::VarBinaryMax(None),
        SqlType::Binary(_, length) => SqlType::Binary(None, length),
        SqlType::Image(_) => SqlType::Image(None),
        SqlType::Char(_, length) => SqlType::Char(None, length),
        SqlType::NChar(_, length) => SqlType::NChar(None, length),
        SqlType::Text(_) => SqlType::Text(None),
        SqlType::NText(_) => SqlType::NText(None),
        SqlType::Json(_) => SqlType::Json(None),
        SqlType::Xml(_) => SqlType::Xml(None),
        SqlType::Uuid(_) => SqlType::Uuid(None),
        SqlType::Vector(_, dimensions, base_type) => SqlType::Vector(None, dimensions, base_type),
        SqlType::Variant(inner) => SqlType::Variant(Box::new(into_typed_null(*inner))),
        SqlType::Table(name, _) => SqlType::Table(name, None),
        SqlType::Udt(name, _) => SqlType::Udt(name, None),
    }
}

// Option<T>: None becomes the SQL NULL of the same type.
impl<T: ToSql + Default> ToSql for Option<T> {
    fn to_sql(&self) -> SqlType {
        match self {
            Some(v) => v.to_sql(),
            None => into_typed_null(T::default().to_sql()),
        }
    }

    fn debug_fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Some(v) => {
                f.write_str("Some(")?;
                v.debug_fmt(f)?;
                f.write_str(")")
            }
            None => f.write_str("None"),
        }
    }
}

impl ToSql for serde_json::Value {
    fn to_sql(&self) -> SqlType {
        match self {
            serde_json::Value::Null => SqlType::NVarchar(None, 4000),
            serde_json::Value::Bool(value) => SqlType::Bit(Some(*value)),
            serde_json::Value::Number(number) => {
                if let Some(value) = number.as_i64() {
                    SqlType::BigInt(Some(value))
                } else if let Some(value) = number.as_f64() {
                    SqlType::Float(Some(value))
                } else {
                    SqlType::Float(Some(f64::NAN))
                }
            }
            serde_json::Value::String(value) => nvarchar_from_string(Some(value.clone())),
            serde_json::Value::Array(_) | serde_json::Value::Object(_) => {
                nvarchar_from_string(Some(self.to_string()))
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Binary
// ---------------------------------------------------------------------------

impl ToSql for Vec<u8> {
    fn to_sql(&self) -> SqlType {
        SqlType::VarBinaryMax(Some(self.clone()))
    }
}

impl ToSql for &[u8] {
    fn to_sql(&self) -> SqlType {
        SqlType::VarBinaryMax(Some(self.to_vec()))
    }
}

// ---------------------------------------------------------------------------
// chrono date/time
// ---------------------------------------------------------------------------

// chrono is an unconditional dep of the driver; FromSql for these types
// already lives in `row.rs`, so the ToSql side is provided unconditionally too.
mod chrono_to_sql {
    use super::{SqlType, ToSql};
    use chrono::{
        DateTime, Datelike, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, Timelike, Utc,
    };
    use mssql_tds::datatypes::column_values::{
        DEFAULT_VARTIME_SCALE, SqlDate, SqlDateTime2, SqlDateTimeOffset, SqlTime,
    };

    fn naive_date_to_sql(d: &NaiveDate) -> SqlDate {
        // SqlDate stores days where 0 = 0001-01-01; chrono's num_days_from_ce
        // counts 0001-01-01 as day 1.
        let days = u32::try_from(d.num_days_from_ce() - 1)
            .expect("date out of SQL Server DATE range (0001-01-01..=9999-12-31)");
        SqlDate::create(days).expect("date out of SQL Server DATE range (0001-01-01..=9999-12-31)")
    }

    fn naive_time_to_sql(t: &NaiveTime) -> SqlTime {
        // SqlTime.time_nanoseconds is actually in 100-nanosecond units (mirrors
        // the FromSql side in row.rs).
        let nanos_since_midnight =
            (t.num_seconds_from_midnight() as u64) * 1_000_000_000 + t.nanosecond() as u64;
        SqlTime {
            time_nanoseconds: nanos_since_midnight / 100,
            scale: DEFAULT_VARTIME_SCALE,
        }
    }

    fn naive_dt_to_sql(dt: &NaiveDateTime) -> SqlDateTime2 {
        SqlDateTime2 {
            days: naive_date_to_sql(&dt.date()).get_days(),
            time: naive_time_to_sql(&dt.time()),
        }
    }

    impl ToSql for NaiveDate {
        fn to_sql(&self) -> SqlType {
            SqlType::Date(Some(naive_date_to_sql(self)))
        }
    }

    impl ToSql for NaiveTime {
        fn to_sql(&self) -> SqlType {
            SqlType::Time(Some(naive_time_to_sql(self)))
        }
    }

    impl ToSql for NaiveDateTime {
        fn to_sql(&self) -> SqlType {
            SqlType::DateTime2(Some(naive_dt_to_sql(self)))
        }
    }

    impl ToSql for DateTime<FixedOffset> {
        fn to_sql(&self) -> SqlType {
            // Storage matches FromSql: dt2 holds the UTC components, offset is
            // the original tz offset in minutes.
            let datetime2 = naive_dt_to_sql(&self.naive_utc());
            let offset = (self.offset().local_minus_utc() / 60) as i16;
            SqlType::DateTimeOffset(Some(SqlDateTimeOffset { datetime2, offset }))
        }
    }

    impl ToSql for DateTime<Utc> {
        fn to_sql(&self) -> SqlType {
            let datetime2 = naive_dt_to_sql(&self.naive_utc());
            SqlType::DateTimeOffset(Some(SqlDateTimeOffset {
                datetime2,
                offset: 0,
            }))
        }
    }
}

#[cfg(feature = "time")]
mod time_to_sql {
    use super::{SqlType, ToSql};
    use chrono::Datelike;
    use mssql_tds::datatypes::column_values::{
        DEFAULT_VARTIME_SCALE, SqlDate, SqlDateTime2, SqlDateTimeOffset, SqlTime,
    };
    use time::{Date, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

    fn date_to_sql(d: Date) -> SqlDate {
        let chrono_date =
            chrono::NaiveDate::from_ymd_opt(d.year(), u8::from(d.month()) as u32, d.day() as u32)
                .expect("date out of SQL Server DATE range (0001-01-01..=9999-12-31)");
        let days = u32::try_from(chrono_date.num_days_from_ce() - 1)
            .expect("date out of SQL Server DATE range (0001-01-01..=9999-12-31)");
        SqlDate::create(days).expect("date out of SQL Server DATE range (0001-01-01..=9999-12-31)")
    }

    fn time_to_sql(t: Time) -> SqlTime {
        let nanos_since_midnight = (t.hour() as u64) * 3_600_000_000_000
            + (t.minute() as u64) * 60_000_000_000
            + (t.second() as u64) * 1_000_000_000
            + t.nanosecond() as u64;
        SqlTime {
            time_nanoseconds: nanos_since_midnight / 100,
            scale: DEFAULT_VARTIME_SCALE,
        }
    }

    fn primitive_dt_to_sql(dt: PrimitiveDateTime) -> SqlDateTime2 {
        SqlDateTime2 {
            days: date_to_sql(dt.date()).get_days(),
            time: time_to_sql(dt.time()),
        }
    }

    impl ToSql for Date {
        fn to_sql(&self) -> SqlType {
            SqlType::Date(Some(date_to_sql(*self)))
        }
    }

    impl ToSql for Time {
        fn to_sql(&self) -> SqlType {
            SqlType::Time(Some(time_to_sql(*self)))
        }
    }

    impl ToSql for PrimitiveDateTime {
        fn to_sql(&self) -> SqlType {
            SqlType::DateTime2(Some(primitive_dt_to_sql(*self)))
        }
    }

    impl ToSql for OffsetDateTime {
        fn to_sql(&self) -> SqlType {
            let offset = self.offset();
            let utc = self.to_offset(UtcOffset::UTC);
            SqlType::DateTimeOffset(Some(SqlDateTimeOffset {
                datetime2: primitive_dt_to_sql(PrimitiveDateTime::new(utc.date(), utc.time())),
                offset: (offset.whole_seconds() / 60) as i16,
            }))
        }
    }
}

#[cfg(feature = "jiff")]
mod jiff_to_sql {
    use super::{SqlType, ToSql};
    use chrono::Datelike;
    use jiff::{Timestamp, Zoned, civil, tz::TimeZone};
    use mssql_tds::datatypes::column_values::{
        DEFAULT_VARTIME_SCALE, SqlDate, SqlDateTime2, SqlDateTimeOffset, SqlTime,
    };

    fn date_to_sql(d: civil::Date) -> SqlDate {
        let chrono_date = chrono::NaiveDate::from_ymd_opt(
            i32::from(d.year()),
            u32::try_from(d.month()).expect("jiff months are in 1..=12"),
            u32::try_from(d.day()).expect("jiff days are in 1..=31"),
        )
        .expect("date out of SQL Server DATE range (0001-01-01..=9999-12-31)");
        let days = u32::try_from(chrono_date.num_days_from_ce() - 1)
            .expect("date out of SQL Server DATE range (0001-01-01..=9999-12-31)");
        SqlDate::create(days).expect("date out of SQL Server DATE range (0001-01-01..=9999-12-31)")
    }

    fn time_to_sql(t: civil::Time) -> SqlTime {
        let nanos_since_midnight = u64::try_from(t.hour()).expect("jiff hours are in 0..=23")
            * 3_600_000_000_000
            + u64::try_from(t.minute()).expect("jiff minutes are in 0..=59") * 60_000_000_000
            + u64::try_from(t.second()).expect("jiff seconds are in 0..=59") * 1_000_000_000
            + u64::try_from(t.subsec_nanosecond()).expect("jiff nanoseconds are nonnegative");
        SqlTime {
            time_nanoseconds: nanos_since_midnight / 100,
            scale: DEFAULT_VARTIME_SCALE,
        }
    }

    fn datetime_to_sql(dt: civil::DateTime) -> SqlDateTime2 {
        SqlDateTime2 {
            days: date_to_sql(dt.date()).get_days(),
            time: time_to_sql(dt.time()),
        }
    }

    fn timestamp_to_sql(timestamp: Timestamp, offset_minutes: i16) -> SqlDateTimeOffset {
        let utc = TimeZone::UTC.to_datetime(timestamp);
        SqlDateTimeOffset {
            datetime2: datetime_to_sql(utc),
            offset: offset_minutes,
        }
    }

    impl ToSql for civil::Date {
        fn to_sql(&self) -> SqlType {
            SqlType::Date(Some(date_to_sql(*self)))
        }
    }

    impl ToSql for civil::Time {
        fn to_sql(&self) -> SqlType {
            SqlType::Time(Some(time_to_sql(*self)))
        }
    }

    impl ToSql for civil::DateTime {
        fn to_sql(&self) -> SqlType {
            SqlType::DateTime2(Some(datetime_to_sql(*self)))
        }
    }

    impl ToSql for Timestamp {
        fn to_sql(&self) -> SqlType {
            SqlType::DateTimeOffset(Some(timestamp_to_sql(*self, 0)))
        }
    }

    impl ToSql for Zoned {
        fn to_sql(&self) -> SqlType {
            SqlType::DateTimeOffset(Some(timestamp_to_sql(
                self.timestamp(),
                (self.offset().seconds() / 60) as i16,
            )))
        }
    }
}

fn encode_string_parameters(sql_type: SqlType, unicode: bool) -> SqlType {
    if unicode {
        return sql_type;
    }

    match sql_type {
        SqlType::NVarchar(value, len) => SqlType::Varchar(value, len),
        SqlType::NVarcharMax(value) => SqlType::VarcharMax(value),
        other => other,
    }
}

/// Build a `Vec<RpcParameter>` from a slice of ToSql values, using positional
/// naming (@P1, @P2, ...) like tiberius.
pub fn build_params(params: &[&dyn ToSql]) -> Vec<RpcParameter> {
    build_params_with_string_encoding(params, true)
}

pub(crate) fn build_params_with_string_encoding(
    params: &[&dyn ToSql],
    unicode: bool,
) -> Vec<RpcParameter> {
    params
        .iter()
        .enumerate()
        .map(|(i, p)| {
            RpcParameter::new(
                Some(format!("@P{}", i + 1)),
                StatusFlags::NONE,
                encode_string_parameters(p.to_sql(), unicode),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn ensure_equal<T: PartialEq + std::fmt::Debug>(actual: T, expected: T) -> TestResult {
        if actual != expected {
            return Err(format!("expected {expected:?}, got {actual:?}").into());
        }
        Ok(())
    }

    fn ensure(condition: bool, message: &'static str) -> TestResult {
        if !condition {
            return Err(message.into());
        }
        Ok(())
    }

    #[test]
    fn to_sql_primitives() -> TestResult {
        ensure(
            matches!(42i32.to_sql(), SqlType::Int(Some(42))),
            "expected int",
        )?;
        ensure(
            matches!(255u8.to_sql(), SqlType::TinyInt(Some(255))),
            "expected tinyint",
        )?;
        ensure(
            matches!((-12i16).to_sql(), SqlType::SmallInt(Some(-12))),
            "expected smallint",
        )?;
        ensure(
            matches!(i64::MAX.to_sql(), SqlType::BigInt(Some(i64::MAX))),
            "expected bigint",
        )?;
        ensure(
            matches!(true.to_sql(), SqlType::Bit(Some(true))),
            "expected bit",
        )?;
        ensure(
            matches!(1.5f32.to_sql(), SqlType::Real(Some(1.5))),
            "expected real",
        )?;
        ensure(
            matches!(2.5f64.to_sql(), SqlType::Float(Some(2.5))),
            "expected float",
        )?;
        ensure(
            matches!(Some(7i32).to_sql(), SqlType::Int(Some(7))),
            "expected optional int",
        )?;
        ensure(
            matches!(None::<i32>.to_sql(), SqlType::Int(None)),
            "expected typed SQL NULL",
        )?;
        for value in ["hello".to_sql(), "hello".to_owned().to_sql()] {
            let SqlType::NVarchar(Some(value), 4000) = value else {
                return Err("expected nvarchar".into());
            };
            ensure_equal(value.to_utf8_string().as_str(), "hello")?;
        }
        let bytes = vec![0, 255, 42];
        for value in [bytes.to_sql(), bytes.as_slice().to_sql()] {
            ensure(
                matches!(value, SqlType::VarBinaryMax(Some(v)) if v == bytes),
                "expected binary bytes",
            )?;
        }
        Ok(())
    }

    #[test]
    fn parameter_debug_preserves_types_and_option_values() {
        let id = uuid::Uuid::nil();
        let owned = "owned".to_owned();
        let params: &[&dyn ToSql] = &[
            &true,
            &255u8,
            &-12i16,
            &42i64,
            &1.5f32,
            &2.5f64,
            &owned,
            &Some(7i32),
            &id,
        ];
        assert_eq!(
            format!("{:?}", DebugParams(params)),
            r#"[true, 255, -12, 42, 1.5, 2.5, "owned", Some(7), <sql param>]"#
        );
        assert!(matches!(id.to_sql(), SqlType::Uuid(Some(value)) if value == id));
        assert!(matches!(
            encode_string_parameters(SqlType::NVarcharMax(None), false),
            SqlType::VarcharMax(None)
        ));
        assert!(matches!(
            encode_string_parameters(42i32.to_sql(), false),
            SqlType::Int(Some(42))
        ));
    }

    #[tokio::test]
    async fn buffered_stream_skips_empty_sets_and_tracks_remaining_rows() -> TestResult {
        use futures_core::Stream;
        use futures_util::StreamExt;
        use mssql_tds::datatypes::column_values::ColumnValues;

        let row = |n| Row::from_tds(&[], vec![ColumnValues::Int(n)]);
        let mut stream = QueryResult {
            result_sets: vec![vec![], vec![row(1), row(2)], vec![], vec![row(3)], vec![]],
        }
        .into_row_stream();
        for expected in 1..=3 {
            let remaining = 4 - usize::try_from(expected)?;
            ensure_equal(stream.size_hint(), (remaining, Some(remaining)))?;
            ensure_equal(
                stream
                    .next()
                    .await
                    .ok_or("expected another row")??
                    .get::<i32, _>(0),
                Some(expected),
            )?;
        }
        ensure_equal(stream.size_hint(), (0, Some(0)))?;
        ensure(stream.next().await.is_none(), "expected end of stream")?;
        ensure(
            stream.next().await.is_none(),
            "stream must remain exhausted",
        )?;
        ensure(
            QueryResult::empty()
                .into_row_stream()
                .next()
                .await
                .is_none(),
            "empty results must not yield rows",
        )?;
        let result = ExecuteResult {
            counts: vec![0, 2, 3],
        };
        ensure_equal(result.rows_affected(), &[0, 2, 3])?;
        ensure_equal(result.total(), 5)?;
        let counts = ExecuteResult::into_iter(result).collect::<Vec<_>>();
        ensure_equal(counts, vec![0, 2, 3])?;
        let result = ExecuteResult {
            counts: vec![0, 2, 3],
        };
        let counts = IntoIterator::into_iter(result).collect::<Vec<_>>();
        ensure_equal(counts, vec![0, 2, 3])?;
        let mut iterated = Vec::new();
        for count in (ExecuteResult {
            counts: vec![0, 2, 3],
        }) {
            iterated.push(count);
        }
        ensure_equal(iterated, vec![0, 2, 3])?;
        Ok(())
    }

    #[cfg(feature = "time")]
    #[test]
    fn time_temporals_roundtrip_with_fractional_seconds_and_offset() -> TestResult {
        use crate::FromSql;
        use mssql_tds::datatypes::column_values::ColumnValues;
        let time = time::Time::from_hms_nano(23, 45, 56, 123_456_700)?;
        let date = time::Date::from_calendar_date(2024, time::Month::February, 29)?;
        let dt = time::PrimitiveDateTime::new(date, time);
        let SqlType::Time(Some(value)) = time.to_sql() else {
            return Err("expected time".into());
        };
        ensure_equal(time::Time::from_sql(&ColumnValues::Time(value)), Some(time))?;
        let SqlType::DateTime2(Some(value)) = dt.to_sql() else {
            return Err("expected datetime2".into());
        };
        ensure_equal(
            time::PrimitiveDateTime::from_sql(&ColumnValues::DateTime2(value)),
            Some(dt),
        )?;
        for offset in [-330, 0, 345] {
            let dt = dt.assume_offset(time::UtcOffset::from_whole_seconds(offset * 60)?);
            let SqlType::DateTimeOffset(Some(value)) = dt.to_sql() else {
                return Err("expected datetimeoffset".into());
            };
            ensure_equal(value.offset, offset as i16)?;
            let actual = time::OffsetDateTime::from_sql(&ColumnValues::DateTimeOffset(value))
                .ok_or("expected decoded datetimeoffset")?;
            ensure_equal(actual, dt)?;
            ensure_equal(actual.offset(), dt.offset())?;
            ensure_equal(actual.date(), dt.date())?;
            ensure_equal(actual.time(), dt.time())?;
        }
        ensure_equal(time::Time::from_sql(&ColumnValues::Null), None)?;
        ensure_equal(time::PrimitiveDateTime::from_sql(&ColumnValues::Null), None)?;
        ensure_equal(time::OffsetDateTime::from_sql(&ColumnValues::Null), None)?;
        Ok(())
    }

    #[cfg(feature = "jiff")]
    #[test]
    fn jiff_temporals_roundtrip_with_fractional_seconds_and_offset() -> TestResult {
        use crate::FromSql;
        use mssql_tds::datatypes::column_values::ColumnValues;
        let dt = jiff::civil::DateTime::new(2024, 2, 29, 23, 45, 56, 123_456_700)
            .map_err(|error| error.to_string())?;
        let SqlType::Time(Some(value)) = dt.time().to_sql() else {
            return Err("expected time".into());
        };
        ensure_equal(
            jiff::civil::Time::from_sql(&ColumnValues::Time(value)),
            Some(dt.time()),
        )?;
        let SqlType::DateTime2(Some(value)) = dt.to_sql() else {
            return Err("expected datetime2".into());
        };
        ensure_equal(
            jiff::civil::DateTime::from_sql(&ColumnValues::DateTime2(value)),
            Some(dt),
        )?;
        for offset in [-330, 0, 345] {
            let zone = jiff::tz::Offset::from_seconds(offset * 60)
                .map_err(|error| error.to_string())?
                .to_time_zone();
            let zoned = dt.to_zoned(zone).map_err(|error| error.to_string())?;
            let SqlType::DateTimeOffset(Some(value)) = zoned.to_sql() else {
                return Err("expected datetimeoffset".into());
            };
            ensure_equal(value.offset, offset as i16)?;
            let actual = jiff::Zoned::from_sql(&ColumnValues::DateTimeOffset(value))
                .ok_or("expected decoded datetimeoffset")?;
            ensure_equal(actual.timestamp(), zoned.timestamp())?;
            ensure_equal(actual.offset(), zoned.offset())?;
            ensure_equal(actual.datetime(), dt)?;
            let SqlType::DateTimeOffset(Some(value)) = zoned.timestamp().to_sql() else {
                return Err("expected datetimeoffset".into());
            };
            ensure_equal(value.offset, 0)?;
            ensure_equal(
                jiff::Timestamp::from_sql(&ColumnValues::DateTimeOffset(value)),
                Some(zoned.timestamp()),
            )?;
        }
        ensure_equal(jiff::civil::Time::from_sql(&ColumnValues::Null), None)?;
        ensure_equal(jiff::civil::DateTime::from_sql(&ColumnValues::Null), None)?;
        ensure_equal(jiff::Timestamp::from_sql(&ColumnValues::Null), None)?;
        ensure(
            jiff::Zoned::from_sql(&ColumnValues::Null).is_none(),
            "SQL NULL must not decode to a zoned datetime",
        )?;
        Ok(())
    }

    #[test]
    fn build_params_positional_naming() {
        let params = build_params(&[&1i32, &"test"]);
        assert_eq!(params.len(), 2);
        // name field is pub(crate) in mssql-tds, so we just verify count
    }

    #[test]
    fn debug_params_formats_values() {
        let none = None::<i32>;
        let params: &[&dyn ToSql] = &[&1i32, &"test", &none];
        assert_eq!(format!("{:?}", DebugParams(params)), r#"[1, "test", None]"#);
    }

    #[test]
    fn option_none_preserves_the_inner_parameter_type() {
        use mssql_tds::datatypes::sql_tvp::TvpTypeName;
        use mssql_tds::datatypes::sql_udt::UdtTypeName;
        use mssql_tds::datatypes::sqldatatypes::VectorBaseType;

        assert!(matches!(None::<bool>.to_sql(), SqlType::Bit(None)));
        assert!(matches!(None::<u8>.to_sql(), SqlType::TinyInt(None)));
        assert!(matches!(None::<i16>.to_sql(), SqlType::SmallInt(None)));
        assert!(matches!(None::<i32>.to_sql(), SqlType::Int(None)));
        assert!(matches!(None::<i64>.to_sql(), SqlType::BigInt(None)));
        assert!(matches!(None::<f32>.to_sql(), SqlType::Real(None)));
        assert!(matches!(None::<f64>.to_sql(), SqlType::Float(None)));
        assert!(matches!(
            None::<String>.to_sql(),
            SqlType::NVarchar(None, 4000)
        ));
        assert!(matches!(
            None::<Vec<u8>>.to_sql(),
            SqlType::VarBinaryMax(None)
        ));
        assert!(matches!(None::<uuid::Uuid>.to_sql(), SqlType::Uuid(None)));
        assert!(matches!(
            None::<rust_decimal::Decimal>.to_sql(),
            SqlType::Numeric(None)
        ));
        assert!(matches!(
            None::<chrono::NaiveDate>.to_sql(),
            SqlType::Date(None)
        ));
        assert!(matches!(
            None::<chrono::NaiveTime>.to_sql(),
            SqlType::Time(None)
        ));
        assert!(matches!(
            None::<chrono::NaiveDateTime>.to_sql(),
            SqlType::DateTime2(None)
        ));
        assert!(matches!(
            into_typed_null(SqlType::Variant(Box::new(SqlType::Int(Some(1))))),
            SqlType::Variant(inner) if matches!(*inner, SqlType::Int(None))
        ));

        let table_name = TvpTypeName::new(Some("dbo".into()), "Items".into());
        let udt_name = UdtTypeName::new(
            Some("master".into()),
            Some("sys".into()),
            "hierarchyid".into(),
        );
        let remaining = [
            (SqlType::Decimal(None), SqlType::Decimal(None)),
            (SqlType::Money(None), SqlType::Money(None)),
            (SqlType::SmallMoney(None), SqlType::SmallMoney(None)),
            (SqlType::DateTimeOffset(None), SqlType::DateTimeOffset(None)),
            (SqlType::SmallDateTime(None), SqlType::SmallDateTime(None)),
            (SqlType::DateTime(None), SqlType::DateTime(None)),
            (SqlType::NVarcharMax(None), SqlType::NVarcharMax(None)),
            (SqlType::Varchar(None, 12), SqlType::Varchar(None, 12)),
            (SqlType::VarcharMax(None), SqlType::VarcharMax(None)),
            (SqlType::VarBinary(None, 12), SqlType::VarBinary(None, 12)),
            (SqlType::Binary(None, 12), SqlType::Binary(None, 12)),
            (SqlType::Image(None), SqlType::Image(None)),
            (SqlType::Char(None, 12), SqlType::Char(None, 12)),
            (SqlType::NChar(None, 12), SqlType::NChar(None, 12)),
            (SqlType::Text(None), SqlType::Text(None)),
            (SqlType::NText(None), SqlType::NText(None)),
            (SqlType::Json(None), SqlType::Json(None)),
            (SqlType::Xml(None), SqlType::Xml(None)),
            (
                SqlType::Vector(None, 3, VectorBaseType::Float32),
                SqlType::Vector(None, 3, VectorBaseType::Float32),
            ),
            (
                SqlType::Table(table_name.clone(), None),
                SqlType::Table(table_name, None),
            ),
            (
                SqlType::Udt(udt_name.clone(), Some(vec![1, 2, 3])),
                SqlType::Udt(udt_name, None),
            ),
        ];
        for (input, expected) in remaining {
            assert_eq!(into_typed_null(input), expected);
        }
    }

    #[test]
    fn decimal_to_sql_basic() {
        use rust_decimal::Decimal;
        let d = Decimal::new(12345, 2); // 123.45
        let sql_type = d.to_sql();
        // Should be Numeric type
        assert!(matches!(sql_type, SqlType::Numeric(Some(_))));
    }

    #[test]
    fn decimal_roundtrips_through_column_data() -> TestResult {
        use mssql_tds::datatypes::column_values::ColumnValues;
        use rust_decimal::Decimal;

        let original = Decimal::new(12345, 2); // 123.45
        let sql_type = original.to_sql();

        // Extract DecimalParts from SqlType
        let decimal_parts = match sql_type {
            SqlType::Numeric(Some(dp)) => dp,
            _ => return Err("expected SqlType::Numeric with DecimalParts".into()),
        };

        // Wrap in ColumnValues::Numeric
        let column_val = ColumnValues::Numeric(decimal_parts);

        // Convert back using FromSql
        let roundtripped: Option<Decimal> = crate::FromSql::from_sql(&column_val);
        ensure_equal(roundtripped, Some(original))?;
        Ok(())
    }

    #[test]
    fn decimal_zero_roundtrips() -> TestResult {
        use mssql_tds::datatypes::column_values::ColumnValues;
        use rust_decimal::Decimal;

        let original = Decimal::new(0, 0);
        let sql_type = original.to_sql();
        let decimal_parts = match sql_type {
            SqlType::Numeric(Some(dp)) => dp,
            _ => return Err("expected SqlType::Numeric".into()),
        };
        let column_val = ColumnValues::Numeric(decimal_parts);
        let roundtripped: Option<Decimal> = crate::FromSql::from_sql(&column_val);
        ensure_equal(roundtripped, Some(original))?;
        Ok(())
    }

    #[test]
    fn decimal_negative_roundtrips() -> TestResult {
        use mssql_tds::datatypes::column_values::ColumnValues;
        use rust_decimal::Decimal;

        let original = Decimal::new(-99999, 4); // -9.9999
        let sql_type = original.to_sql();
        let decimal_parts = match sql_type {
            SqlType::Numeric(Some(dp)) => dp,
            _ => return Err("expected SqlType::Numeric".into()),
        };
        let column_val = ColumnValues::Numeric(decimal_parts);
        let roundtripped: Option<Decimal> = crate::FromSql::from_sql(&column_val);
        ensure_equal(roundtripped, Some(original))?;
        Ok(())
    }

    #[test]
    fn decimal_high_precision_roundtrips() -> TestResult {
        use mssql_tds::datatypes::column_values::ColumnValues;
        use rust_decimal::Decimal;

        // rust_decimal supports up to 28 digits of precision with i64 mantissa
        let original = Decimal::new(9223372036854775807i64, 10); // i64::MAX
        let sql_type = original.to_sql();
        let decimal_parts = match sql_type {
            SqlType::Numeric(Some(dp)) => dp,
            _ => return Err("expected SqlType::Numeric".into()),
        };
        let column_val = ColumnValues::Numeric(decimal_parts);
        let roundtripped: Option<Decimal> = crate::FromSql::from_sql(&column_val);
        ensure_equal(roundtripped, Some(original))?;
        Ok(())
    }

    #[test]
    fn decimal_debug_fmt_displays_value() {
        use rust_decimal::Decimal;

        let d = Decimal::new(12345, 2);
        let params: Vec<&dyn ToSql> = vec![&d];
        let output = format!("{:?}", DebugParams(&params));
        assert!(output.contains("123.45"));
    }

    #[test]
    fn decimal_precision_boundary_uses_fallback() {
        use rust_decimal::Decimal;
        use std::str::FromStr;

        // A value with 28 significant digits — the maximum for rust_decimal.
        let d = Decimal::from_str("9999999999999999999999999999")
            .expect("28 significant digits fit a decimal mantissa");
        let sql_type = d.to_sql();
        assert!(matches!(sql_type, SqlType::Numeric(Some(_))));
    }

    #[test]
    fn decimal_precision_is_exact_near_powers_of_ten() -> TestResult {
        for (text, precision) in [
            ("0", 1),
            ("9", 1),
            ("10", 2),
            ("9999999999999999999999999999", 28),
            ("10000000000000000000000000000", 29),
            ("-9999999999999999999999999999", 28),
        ] {
            let decimal: rust_decimal::Decimal = text.parse()?;
            let SqlType::Numeric(Some(parts)) = decimal.to_sql() else {
                return Err("expected a non-null SQL numeric".into());
            };
            ensure_equal(parts.precision, precision)?;
            ensure_equal(
                crate::FromSql::from_sql(
                    &mssql_tds::datatypes::column_values::ColumnValues::Numeric(parts),
                ),
                Some(decimal),
            )?;
        }
        Ok(())
    }

    #[test]
    fn decimal_to_sql_type_fallback_on_low_precision() {
        // Call with deliberately too-low precision to trigger the fallback path
        let result = super::decimal_to_sql_type("12345.67", 3, 2);
        // The initial from_string(precision=3) fails because 7 digits > 3,
        // then the fallback with precision=38 succeeds.
        assert!(matches!(result, SqlType::Numeric(Some(_))));
    }

    #[test]
    fn decimal_to_sql_type_total_failure_returns_none() {
        // Invalid decimal string that can't be parsed at all
        let result = super::decimal_to_sql_type("not_a_number", 10, 2);
        assert!(matches!(result, SqlType::Numeric(None)));
    }

    #[test]
    fn decimal_scale_exceeds_precision_handled() {
        use rust_decimal::Decimal;

        // Scale=28, mantissa=1 → "0.0000000000000000000000000001"
        // precision from ilog10(1)=0 → 1, but scale=28, so precision.max(28)=28
        let d = Decimal::new(1, 28);
        let sql_type = d.to_sql();
        assert!(matches!(sql_type, SqlType::Numeric(Some(_))));
    }

    #[test]
    fn chrono_datetime_sql_date_boundaries_roundtrip() -> TestResult {
        use mssql_tds::datatypes::column_values::ColumnValues;

        for (year, month, day) in [(1, 1, 1), (9999, 12, 31)] {
            let datetime = chrono::NaiveDate::from_ymd_opt(year, month, day)
                .and_then(|date| date.and_hms_opt(0, 0, 0))
                .ok_or("expected a valid SQL date boundary")?;
            let SqlType::DateTime2(Some(value)) = datetime.to_sql() else {
                return Err("expected SQL datetime2".into());
            };
            ensure_equal(
                crate::FromSql::from_sql(&ColumnValues::DateTime2(value)),
                Some(datetime),
            )?;
        }
        Ok(())
    }

    #[test]
    #[should_panic(expected = "date out of SQL Server DATE range")]
    fn chrono_datetime_rejects_dates_before_sql_epoch() {
        let datetime = chrono::NaiveDate::from_ymd_opt(0, 12, 31)
            .and_then(|date| date.and_hms_opt(0, 0, 0))
            .expect("chrono supports dates before the SQL epoch");
        let _ = datetime.to_sql();
    }

    #[test]
    #[should_panic(expected = "date out of SQL Server DATE range")]
    fn chrono_datetime_rejects_dates_after_sql_range() {
        let datetime = chrono::NaiveDate::from_ymd_opt(10000, 1, 1)
            .and_then(|date| date.and_hms_opt(0, 0, 0))
            .expect("chrono supports years after 9999");
        let _ = datetime.to_sql();
    }

    #[cfg(feature = "time")]
    #[test]
    fn time_date_roundtrips_through_column_data() -> TestResult {
        use mssql_tds::datatypes::column_values::ColumnValues;
        let date = time::Date::from_calendar_date(2024, time::Month::February, 29)?;
        let SqlType::Date(Some(sql_date)) = date.to_sql() else {
            return Err("expected SQL date".into());
        };
        let value = ColumnValues::Date(sql_date);
        ensure_equal(crate::FromSql::from_sql(&value), Some(date))?;
        Ok(())
    }

    #[cfg(feature = "jiff")]
    #[test]
    fn jiff_date_roundtrips_through_column_data() -> TestResult {
        use mssql_tds::datatypes::column_values::ColumnValues;
        let date = jiff::civil::date(2024, 2, 29);
        let SqlType::Date(Some(sql_date)) = date.to_sql() else {
            return Err("expected SQL date".into());
        };
        let value = ColumnValues::Date(sql_date);
        ensure_equal(crate::FromSql::from_sql(&value), Some(date))?;
        Ok(())
    }

    #[test]
    fn string_parameter_encoding_can_use_varchar() {
        let ty = encode_string_parameters("hello".to_sql(), false);
        assert!(matches!(ty, SqlType::Varchar(_, 4000)));
    }

    #[test]
    fn string_parameter_encoding_defaults_to_nvarchar() {
        let ty = encode_string_parameters("hello".to_sql(), true);
        assert!(matches!(ty, SqlType::NVarchar(_, 4000)));
    }

    #[test]
    fn native_string_and_json_parameters_use_utf16_length() -> TestResult {
        for (text, max) in [
            ("a".repeat(4000), false),
            ("a".repeat(4001), true),
            ("😀".repeat(2000), false),
            ("😀".repeat(2001), true),
        ] {
            for value in [text.as_str().to_sql(), text.to_sql(), json!(text).to_sql()] {
                match value {
                    SqlType::NVarchar(Some(value), 4000) if !max => {
                        ensure_equal(value.to_utf8_string().as_str(), text.as_str())?;
                    }
                    SqlType::NVarcharMax(Some(value)) if max => {
                        ensure_equal(value.to_utf8_string().as_str(), text.as_str())?;
                    }
                    _ => {
                        return Err(format!(
                            "incorrect nvarchar type for UTF-16 length {}",
                            text.encode_utf16().count()
                        )
                        .into());
                    }
                }
            }
        }
        for count in [3996, 3997] {
            let value = json!(["a".repeat(count)]);
            let expected = value.to_string();
            ensure_equal(expected.encode_utf16().count(), count + 4)?;
            match value.to_sql() {
                SqlType::NVarchar(Some(actual), 4000) if count == 3996 => {
                    ensure_equal(actual.to_utf8_string(), expected)?;
                }
                SqlType::NVarcharMax(Some(actual)) if count == 3997 => {
                    ensure_equal(actual.to_utf8_string(), expected)?;
                }
                _ => return Err("incorrect nvarchar type for JSON".into()),
            }
        }
        Ok(())
    }

    #[test]
    fn empty_query_result() {
        let qr = QueryResult::empty();
        assert_eq!(qr.result_set_count(), 0);
        assert!(qr.into_first_result().is_empty());
    }

    #[test]
    fn json_value_to_sql_dispatches_by_variant() -> TestResult {
        ensure(
            matches!(
                serde_json::Value::Null.to_sql(),
                SqlType::NVarchar(None, 4000)
            ),
            "expected SQL NULL",
        )?;
        ensure(
            matches!(json!(true).to_sql(), SqlType::Bit(Some(true))),
            "expected JSON bit",
        )?;
        ensure(
            matches!(json!(42).to_sql(), SqlType::BigInt(Some(42))),
            "expected JSON bigint",
        )?;
        ensure(
            matches!(json!(2.5).to_sql(), SqlType::Float(Some(v)) if (v - 2.5).abs() < f64::EPSILON),
            "expected JSON float",
        )?;

        let ty = json!("alice").to_sql();
        ensure(
            matches!(ty, SqlType::NVarchar(_, 4000)),
            "expected nvarchar string",
        )?;
        if let SqlType::NVarchar(Some(value), 4000) = ty {
            ensure_equal(value.to_utf8_string().as_str(), "alice")?;
        } else {
            return Err("expected NVarchar text for string".into());
        }

        let array = json!([1, 2, 3]);
        if let SqlType::NVarchar(Some(value), 4000) = array.to_sql() {
            ensure_equal(value.to_utf8_string(), array.to_string())?;
        } else {
            return Err("expected NVarchar JSON text for array".into());
        }

        let object = json!({"name":"alice"});
        if let SqlType::NVarchar(Some(value), 4000) = object.to_sql() {
            ensure_equal(value.to_utf8_string(), object.to_string())?;
        } else {
            return Err("expected NVarchar JSON text for object".into());
        }
        Ok(())
    }
}
