// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

//! Row type with named and indexed column access, mirroring tiberius' `Row` API.
//!
//! A [`Row`] is returned from [`QueryResult::into_first_result()`](crate::QueryResult::into_first_result)
//! and provides typed access to column values via [`get()`](Row::get).

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock};

use mssql_tds::datatypes::column_values::{
    ColumnValues, SqlDate, SqlDateTime, SqlDateTime2, SqlDateTimeOffset, SqlMoney,
    SqlSmallDateTime, SqlSmallMoney, SqlTime, SqlXml,
};
use mssql_tds::datatypes::decoder::DecimalParts;
use mssql_tds::datatypes::row_writer::RowWriter;
use mssql_tds::datatypes::sql_json::SqlJson;
use mssql_tds::datatypes::sql_string::{EncodingType, SqlString};
use mssql_tds::datatypes::sql_vector::SqlVector;
use mssql_tds::query::metadata::ColumnMetadata;
use uuid::Uuid;

use crate::column::Column;
use crate::error::{Error, Result};

/// Per-result-set schema (column metadata + name index).
///
/// Built once per result set and shared across every [`Row`] in that set
/// via [`Arc`], so streaming N rows from a result set with K columns no
/// longer pays the O(K) `Vec<Column>` + `HashMap<String, usize>` build
/// cost per row.
#[derive(Debug)]
pub struct RowSchema {
    pub(crate) columns: Vec<Column>,
    pub(crate) name_map: HashMap<String, usize>,
}

impl RowSchema {
    /// Build a schema from a slice of mssql-tds column metadata.
    pub fn from_metadata(metadata: &[ColumnMetadata]) -> Arc<Self> {
        let columns: Vec<Column> = metadata.iter().map(Column::from_tds).collect();
        let mut name_map = HashMap::with_capacity(columns.len());
        for (index, column) in columns.iter().enumerate() {
            name_map.entry(column.name.clone()).or_insert(index);
        }
        Arc::new(RowSchema { columns, name_map })
    }
}

/// A single result row with tiberius-style typed column access.
///
/// String values are eagerly decoded from UTF-16 to UTF-8 at construction
/// time, enabling `row.get::<&str, _>("col")` without allocation.
#[derive(Debug, Clone)]
pub struct Row {
    schema: Arc<RowSchema>,
    values: Vec<ColumnValues>,
    result_index: usize,
    /// Pre-decoded UTF-8 strings for &str borrowing support.
    decoded_strings: Vec<Option<String>>,
    compat_values: OnceLock<Vec<crate::ColumnData<'static>>>,
}

impl Row {
    fn from_parts(
        schema: Arc<RowSchema>,
        values: Vec<ColumnValues>,
        result_index: usize,
        decoded_strings: Vec<Option<String>>,
    ) -> Self {
        Self {
            schema,
            values,
            result_index,
            decoded_strings,
            compat_values: OnceLock::new(),
        }
    }

    fn build_compat_values(
        schema: &RowSchema,
        values: &[ColumnValues],
        decoded_strings: &[Option<String>],
    ) -> Vec<crate::ColumnData<'static>> {
        values
            .iter()
            .zip(decoded_strings)
            .enumerate()
            .map(|(index, (value, decoded))| {
                let column_type = schema
                    .columns
                    .get(index)
                    .map_or(crate::ColumnType::Null, Column::column_type);
                crate::compat::conversion::column_data_ref(value, decoded.as_deref(), column_type)
                    .into_owned()
            })
            .collect()
    }

    fn compat_values(&self) -> &[crate::ColumnData<'static>] {
        self.compat_values.get_or_init(|| {
            Self::build_compat_values(&self.schema, &self.values, &self.decoded_strings)
        })
    }

    /// Build a Row reusing a pre-built [`RowSchema`]. Hot path for the
    /// streaming and buffered code paths — clones the `Arc`, never the
    /// underlying `Vec<Column>`/`HashMap`.
    pub fn from_schema(schema: Arc<RowSchema>, values: Vec<ColumnValues>) -> Self {
        let decoded_strings: Vec<Option<String>> = values
            .iter()
            .map(|v| match v {
                ColumnValues::String(s) => Some(s.to_utf8_string()),
                ColumnValues::Xml(x) => Some(x.as_string()),
                ColumnValues::Json(j) => Some(j.as_string()),
                _ => None,
            })
            .collect();
        Self::from_parts(schema, values, 0, decoded_strings)
    }

    /// Build a Row from mssql-tds column metadata and decoded values.
    ///
    /// Convenience wrapper that builds a fresh [`RowSchema`] each call —
    /// prefer [`Row::from_schema`] inside row loops where the schema is
    /// known to be constant for the result set.
    pub fn from_tds(metadata: &[ColumnMetadata], values: Vec<ColumnValues>) -> Self {
        Self::from_schema(RowSchema::from_metadata(metadata), values)
    }

    /// Column metadata for this row.
    pub fn columns(&self) -> &[Column] {
        &self.schema.columns
    }

    /// Iterate over columns and compatibility values in indexed column order.
    pub fn cells(&self) -> impl ExactSizeIterator<Item = (&Column, &crate::ColumnData<'static>)> {
        self.schema.columns.iter().zip(self.compat_values())
    }

    /// Zero-based index of the result set that produced this row.
    pub fn result_index(&self) -> usize {
        self.result_index
    }

    /// Number of columns.
    pub fn len(&self) -> usize {
        self.values.len()
    }

    /// Whether the row has zero columns.
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }

    /// Get a column value by name or index. Returns `None` if the column is NULL
    /// or the type doesn't match. Panics if the column doesn't exist.
    pub fn get<'a, T: FromSql<'a>, I: ColumnIndex>(&'a self, col: I) -> Option<T> {
        self.try_get(col).expect("column not found")
    }

    /// Try to get a column value by name or index, returning a `Result`.
    pub fn try_get<'a, T: FromSql<'a>, I: ColumnIndex>(&'a self, col: I) -> Result<Option<T>> {
        let idx = col.resolve(self)?;
        self.try_get_at(idx)
    }

    /// Get a value using Tiberius-compatible conversion errors.
    ///
    /// Panics when the column is missing or a non-NULL value has the wrong
    /// type. Use [`Row::try_get_compat`] for a recoverable error.
    pub fn get_compat<'a, T: crate::compat::FromSql<'a>, I: ColumnIndex>(
        &'a self,
        col: I,
    ) -> Option<T> {
        self.try_get_compat(col).expect("column conversion failed")
    }

    /// Try to get a value using Tiberius-compatible conversion errors.
    ///
    /// A target-compatible typed SQL NULL returns `Ok(None)`; an incompatible
    /// variant returns [`Error::Conversion`]. The driver-native
    /// [`Row::try_get`] is unchanged.
    pub fn try_get_compat<'a, T: crate::compat::FromSql<'a>, I: ColumnIndex>(
        &'a self,
        col: I,
    ) -> Result<Option<T>> {
        let idx = col.resolve(self)?;
        let compat_values = self.compat_values();
        let value = compat_values
            .get(idx)
            .ok_or(Error::ColumnIndexOutOfBounds {
                index: idx,
                count: compat_values.len(),
            })?;
        T::from_sql(value)
    }

    /// Get a column value by name using case-insensitive lookup. Returns `None`
    /// if the column is NULL or the type doesn't match. Panics if the column
    /// doesn't exist.
    pub fn get_ci<'a, T: FromSql<'a>>(&'a self, name: &str) -> Option<T> {
        self.try_get_ci(name).expect("column not found")
    }

    /// Try to get a column value by name using case-insensitive lookup.
    pub fn try_get_ci<'a, T: FromSql<'a>>(&'a self, name: &str) -> Result<Option<T>> {
        let idx = self.resolve_name_ci(name)?;
        self.try_get_at(idx)
    }

    fn try_get_at<'a, T: FromSql<'a>>(&'a self, idx: usize) -> Result<Option<T>> {
        let value = self.values.get(idx).ok_or(Error::ColumnIndexOutOfBounds {
            index: idx,
            count: self.values.len(),
        })?;
        Ok(T::from_sql_with_str(
            value,
            self.decoded_strings.get(idx).and_then(|s| s.as_deref()),
        ))
    }

    fn resolve_name_ci(&self, name: &str) -> Result<usize> {
        if let Some(idx) = self.column_index(name) {
            return Ok(idx);
        }
        self.schema
            .columns
            .iter()
            .position(|c| c.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| Error::ColumnNotFound(name.to_string()))
    }

    /// Raw access to the underlying ColumnValues at a given index.
    ///
    /// Spatial (`geography`/`geometry`) values use [`ColumnValues::Bytes`]
    /// containing SQL Server's native serialization, not OGC WKB.
    /// Use [`Column::column_type`] to distinguish them from ordinary binary data.
    pub fn raw_value(&self, idx: usize) -> Option<&ColumnValues> {
        self.values.get(idx)
    }

    /// Pre-decoded UTF-8 string at the given index, if any. Used by the
    /// `serde` deserializer.
    #[cfg(feature = "serde")]
    #[inline]
    pub(crate) fn decoded_str_at(&self, idx: usize) -> Option<&str> {
        self.decoded_strings.get(idx).and_then(|s| s.as_deref())
    }

    /// Deserialize this row into a `T` using `serde`. Consumes the row.
    ///
    /// Requires the `serde` cargo feature. See the
    /// [`serde_de`](crate::serde_de) module for the field/type mapping rules.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// # #[cfg(feature = "serde")]
    /// # async fn ex(client: &mut mssql_rs::Client)
    /// #     -> mssql_rs::Result<()> {
    /// use serde::Deserialize;
    /// use mssql_rs::Row;
    ///
    /// #[derive(Deserialize)]
    /// struct User { id: i64, name: String }
    ///
    /// let users: Vec<User> = client.query("SELECT id, name FROM users", &[])
    ///     .await?
    ///     .into_first_result()
    ///     .into_iter()
    ///     .map(Row::deserialize)
    ///     .collect::<mssql_rs::Result<Vec<_>>>()?;
    /// # Ok(()) }
    /// ```
    #[cfg(feature = "serde")]
    pub fn deserialize<T: serde::de::DeserializeOwned>(self) -> Result<T> {
        T::deserialize(crate::serde_de::RowDeserializer::new(&self))
    }

    /// Borrowed-deserialize variant: the resulting value can borrow `&str`
    /// and `&[u8]` cells directly out of this row.
    ///
    /// Requires the `serde` cargo feature.
    #[cfg(feature = "serde")]
    pub fn deserialize_borrowed<'de, T: serde::Deserialize<'de>>(&'de self) -> Result<T> {
        T::deserialize(crate::serde_de::RowDeserializer::new(self))
    }

    /// Look up a column index by name (case-sensitive).
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.schema.name_map.get(name).copied()
    }
}

/// Equality on `Row` compares **column types and cell values**. Names and
/// other server-side metadata (nullability, identity, computed, collation,
/// …) are intentionally ignored so that rows produced from different result
/// sets — or built by tests with placeholder names — can be asserted equal
/// when the data shape matches.
///
/// `Row` does **not** implement `Eq` because [`ColumnValues`] holds floats
/// (`Float`/`Real`), which only satisfy `PartialEq`. Comparisons involving
/// `NaN` follow IEEE-754 semantics: `NaN != NaN`.
///
/// Mirrors the request in prisma/tiberius#402.
///
/// # Example
///
/// ```rust,no_run
/// # fn example(actual: mssql_rs::Row, expected: mssql_rs::Row) {
/// assert_eq!(actual, expected);
/// # }
/// ```
impl PartialEq for Row {
    fn eq(&self, other: &Self) -> bool {
        // Cheap pointer-equality fast path for rows that share an Arc<RowSchema>
        // (the common case inside a single result set).
        let schemas_match = Arc::ptr_eq(&self.schema, &other.schema)
            || (self.schema.columns.len() == other.schema.columns.len()
                && self
                    .schema
                    .columns
                    .iter()
                    .zip(other.schema.columns.iter())
                    .all(|(a, b)| a.column_type == b.column_type));

        schemas_match && self.values == other.values
    }
}

impl IntoIterator for Row {
    type Item = crate::ColumnData<'static>;
    type IntoIter = std::vec::IntoIter<Self::Item>;

    fn into_iter(self) -> Self::IntoIter {
        let Row {
            schema,
            values,
            decoded_strings,
            compat_values,
            ..
        } = self;
        compat_values
            .into_inner()
            .unwrap_or_else(|| Self::build_compat_values(&schema, &values, &decoded_strings))
            .into_iter()
    }
}

// ---------------------------------------------------------------------------
// ColumnIndex — resolve &str or usize to a positional index
// ---------------------------------------------------------------------------

/// Trait for types that can identify a column in a row (by name or index).
pub trait ColumnIndex {
    fn resolve(&self, row: &Row) -> Result<usize>;
}

impl ColumnIndex for usize {
    fn resolve(&self, row: &Row) -> Result<usize> {
        if *self >= row.values.len() {
            return Err(Error::ColumnIndexOutOfBounds {
                index: *self,
                count: row.values.len(),
            });
        }
        Ok(*self)
    }
}

impl ColumnIndex for &str {
    fn resolve(&self, row: &Row) -> Result<usize> {
        row.schema
            .name_map
            .get(*self)
            .copied()
            .ok_or_else(|| Error::ColumnNotFound(self.to_string()))
    }
}

// ---------------------------------------------------------------------------
// FromSql — extract a typed value from ColumnValues
// ---------------------------------------------------------------------------

/// Trait for extracting a Rust type from a `ColumnValues` cell.
pub trait FromSql<'a>: Sized {
    fn from_sql(val: &'a ColumnValues) -> Option<Self>;

    /// Extract with an optional pre-decoded string (used for &str borrowing).
    /// Default delegates to `from_sql`.
    fn from_sql_with_str(val: &'a ColumnValues, _decoded: Option<&'a str>) -> Option<Self> {
        Self::from_sql(val)
    }
}

// Option<T>: NULL → Some(None), value → Some(Some(v))
impl<'a, T: FromSql<'a>> FromSql<'a> for Option<T> {
    fn from_sql(val: &'a ColumnValues) -> Option<Self> {
        match val {
            ColumnValues::Null => Some(None),
            _ => T::from_sql(val).map(Some),
        }
    }

    fn from_sql_with_str(val: &'a ColumnValues, decoded: Option<&'a str>) -> Option<Self> {
        match val {
            ColumnValues::Null => Some(None),
            _ => T::from_sql_with_str(val, decoded).map(Some),
        }
    }
}

impl<'a> FromSql<'a> for bool {
    fn from_sql(val: &'a ColumnValues) -> Option<Self> {
        match val {
            ColumnValues::Bit(b) => Some(*b),
            _ => None,
        }
    }
}

impl<'a> FromSql<'a> for u8 {
    fn from_sql(val: &'a ColumnValues) -> Option<Self> {
        match val {
            ColumnValues::TinyInt(v) => Some(*v),
            _ => None,
        }
    }
}

impl<'a> FromSql<'a> for i16 {
    fn from_sql(val: &'a ColumnValues) -> Option<Self> {
        match val {
            ColumnValues::SmallInt(v) => Some(*v),
            ColumnValues::TinyInt(v) => Some(*v as i16),
            _ => None,
        }
    }
}

impl<'a> FromSql<'a> for i32 {
    fn from_sql(val: &'a ColumnValues) -> Option<Self> {
        match val {
            ColumnValues::Int(v) => Some(*v),
            ColumnValues::SmallInt(v) => Some(*v as i32),
            ColumnValues::TinyInt(v) => Some(*v as i32),
            _ => None,
        }
    }
}

impl<'a> FromSql<'a> for i64 {
    fn from_sql(val: &'a ColumnValues) -> Option<Self> {
        match val {
            ColumnValues::BigInt(v) => Some(*v),
            ColumnValues::Int(v) => Some(*v as i64),
            ColumnValues::SmallInt(v) => Some(*v as i64),
            ColumnValues::TinyInt(v) => Some(*v as i64),
            _ => None,
        }
    }
}

impl<'a> FromSql<'a> for f32 {
    fn from_sql(val: &'a ColumnValues) -> Option<Self> {
        match val {
            ColumnValues::Real(v) => Some(*v),
            _ => None,
        }
    }
}

impl<'a> FromSql<'a> for f64 {
    fn from_sql(val: &'a ColumnValues) -> Option<Self> {
        match val {
            ColumnValues::Float(v) => Some(*v),
            ColumnValues::Real(v) => Some(*v as f64),
            _ => None,
        }
    }
}

impl<'a> FromSql<'a> for String {
    fn from_sql(val: &'a ColumnValues) -> Option<Self> {
        match val {
            ColumnValues::String(s) => Some(s.to_utf8_string()),
            ColumnValues::Xml(x) => Some(x.as_string()),
            ColumnValues::Json(j) => Some(j.as_string()),
            _ => None,
        }
    }
}

impl<'a> FromSql<'a> for serde_json::Value {
    fn from_sql(val: &'a ColumnValues) -> Option<Self> {
        match val {
            ColumnValues::Json(j) => serde_json::from_str(&j.as_string()).ok(),
            ColumnValues::String(s) => serde_json::from_str(&s.to_utf8_string()).ok(),
            _ => None,
        }
    }
}

impl<'a> FromSql<'a> for &'a str {
    fn from_sql(_val: &'a ColumnValues) -> Option<Self> {
        // Can't borrow from ColumnValues directly (UTF-16 internal).
        // Use from_sql_with_str path via Row::get() instead.
        None
    }

    fn from_sql_with_str(_val: &'a ColumnValues, decoded: Option<&'a str>) -> Option<Self> {
        decoded
    }
}

impl<'a> FromSql<'a> for uuid::Uuid {
    fn from_sql(val: &'a ColumnValues) -> Option<Self> {
        match val {
            ColumnValues::Uuid(u) => Some(*u),
            _ => None,
        }
    }
}

/// Reads binary and spatial values as owned bytes. Spatial bytes retain SQL
/// Server's native serialization, including the SRID.
impl<'a> FromSql<'a> for Vec<u8> {
    fn from_sql(val: &'a ColumnValues) -> Option<Self> {
        match val {
            ColumnValues::Bytes(b) => Some(b.clone()),
            _ => None,
        }
    }
}

/// Borrows binary and spatial bytes without copying.
impl<'a> FromSql<'a> for &'a [u8] {
    fn from_sql(val: &'a ColumnValues) -> Option<Self> {
        match val {
            ColumnValues::Bytes(b) => Some(b.as_slice()),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Chrono conversions
// ---------------------------------------------------------------------------

impl<'a> FromSql<'a> for chrono::NaiveDate {
    fn from_sql(val: &'a ColumnValues) -> Option<Self> {
        match val {
            ColumnValues::Date(d) => {
                chrono::NaiveDate::from_num_days_from_ce_opt(d.get_days() as i32 + 1)
            }
            ColumnValues::DateTime2(dt) => {
                chrono::NaiveDate::from_num_days_from_ce_opt(dt.days as i32 + 1)
            }
            ColumnValues::DateTime(dt) => {
                // datetime: days since 1900-01-01
                let base = chrono::NaiveDate::from_ymd_opt(1900, 1, 1)?;
                base.checked_add_signed(chrono::Duration::days(dt.days as i64))
            }
            _ => None,
        }
    }
}

impl<'a> FromSql<'a> for chrono::NaiveTime {
    fn from_sql(val: &'a ColumnValues) -> Option<Self> {
        match val {
            ColumnValues::Time(t) => {
                // time_nanoseconds is actually in 100-nanosecond units despite the field name
                let total_nanos = t.time_nanoseconds * 100;
                let secs = (total_nanos / 1_000_000_000) as u32;
                let nanos = (total_nanos % 1_000_000_000) as u32;
                chrono::NaiveTime::from_num_seconds_from_midnight_opt(secs, nanos)
            }
            _ => None,
        }
    }
}

impl<'a> FromSql<'a> for chrono::NaiveDateTime {
    fn from_sql(val: &'a ColumnValues) -> Option<Self> {
        match val {
            ColumnValues::DateTime2(dt) => {
                let date = chrono::NaiveDate::from_num_days_from_ce_opt(dt.days as i32 + 1)?;
                // time_nanoseconds is in 100-nanosecond units
                let total_nanos = dt.time.time_nanoseconds * 100;
                let secs = (total_nanos / 1_000_000_000) as u32;
                let nanos = (total_nanos % 1_000_000_000) as u32;
                let time = chrono::NaiveTime::from_num_seconds_from_midnight_opt(secs, nanos)?;
                Some(chrono::NaiveDateTime::new(date, time))
            }
            ColumnValues::DateTime(dt) => {
                let base = chrono::NaiveDate::from_ymd_opt(1900, 1, 1)?;
                let date = base.checked_add_signed(chrono::Duration::days(dt.days as i64))?;
                // datetime ticks: 1/300th of a second
                let total_nanos = (u64::from(dt.time) * 1_000_000_000 + 150) / 300;
                let secs = (total_nanos / 1_000_000_000) as u32;
                let nanos = (total_nanos % 1_000_000_000) as u32;
                let time = chrono::NaiveTime::from_num_seconds_from_midnight_opt(secs, nanos)?;
                Some(chrono::NaiveDateTime::new(date, time))
            }
            ColumnValues::SmallDateTime(sdt) => {
                let base = chrono::NaiveDate::from_ymd_opt(1900, 1, 1)?;
                let date = base.checked_add_signed(chrono::Duration::days(sdt.days as i64))?;
                let time =
                    chrono::NaiveTime::from_num_seconds_from_midnight_opt(sdt.time as u32 * 60, 0)?;
                Some(chrono::NaiveDateTime::new(date, time))
            }
            _ => None,
        }
    }
}

impl<'a> FromSql<'a> for chrono::DateTime<chrono::FixedOffset> {
    fn from_sql(val: &'a ColumnValues) -> Option<Self> {
        match val {
            ColumnValues::DateTimeOffset(dto) => {
                let dt2 = &dto.datetime2;
                let date = chrono::NaiveDate::from_num_days_from_ce_opt(dt2.days as i32 + 1)?;
                // time_nanoseconds is in 100-nanosecond units
                let total_nanos = dt2.time.time_nanoseconds * 100;
                let secs = (total_nanos / 1_000_000_000) as u32;
                let nanos = (total_nanos % 1_000_000_000) as u32;
                let time = chrono::NaiveTime::from_num_seconds_from_midnight_opt(secs, nanos)?;
                let naive = chrono::NaiveDateTime::new(date, time);
                let offset = chrono::FixedOffset::east_opt(dto.offset as i32 * 60)?;
                Some(chrono::DateTime::from_naive_utc_and_offset(naive, offset))
            }
            _ => None,
        }
    }
}

#[cfg(feature = "time")]
mod time_from_sql {
    use super::{ColumnValues, FromSql};
    use chrono::{Datelike, Timelike};
    use time::{Date, Month, OffsetDateTime, PrimitiveDateTime, Time, UtcOffset};

    fn date_from_chrono(date: chrono::NaiveDate) -> Option<Date> {
        Date::from_calendar_date(
            date.year(),
            Month::try_from(date.month() as u8).ok()?,
            date.day() as u8,
        )
        .ok()
    }

    fn time_from_chrono(time: chrono::NaiveTime) -> Option<Time> {
        Time::from_hms_nano(
            time.hour() as u8,
            time.minute() as u8,
            time.second() as u8,
            time.nanosecond(),
        )
        .ok()
    }

    impl<'a> FromSql<'a> for Date {
        fn from_sql(val: &'a ColumnValues) -> Option<Self> {
            date_from_chrono(chrono::NaiveDate::from_sql(val)?)
        }
    }

    impl<'a> FromSql<'a> for Time {
        fn from_sql(val: &'a ColumnValues) -> Option<Self> {
            time_from_chrono(chrono::NaiveTime::from_sql(val)?)
        }
    }

    impl<'a> FromSql<'a> for PrimitiveDateTime {
        fn from_sql(val: &'a ColumnValues) -> Option<Self> {
            let dt = chrono::NaiveDateTime::from_sql(val)?;
            Some(PrimitiveDateTime::new(
                date_from_chrono(dt.date())?,
                time_from_chrono(dt.time())?,
            ))
        }
    }

    impl<'a> FromSql<'a> for OffsetDateTime {
        fn from_sql(val: &'a ColumnValues) -> Option<Self> {
            let dt = chrono::DateTime::<chrono::FixedOffset>::from_sql(val)?;
            let offset = UtcOffset::from_whole_seconds(dt.offset().local_minus_utc()).ok()?;
            let utc = PrimitiveDateTime::new(
                date_from_chrono(dt.naive_utc().date())?,
                time_from_chrono(dt.naive_utc().time())?,
            )
            .assume_utc();
            Some(utc.to_offset(offset))
        }
    }
}

#[cfg(feature = "jiff")]
mod jiff_from_sql {
    use super::{ColumnValues, FromSql};
    use chrono::{Datelike, Timelike};
    use jiff::{Timestamp, Zoned, civil, tz::Offset};

    fn date_from_chrono(date: chrono::NaiveDate) -> Option<civil::Date> {
        civil::Date::new(date.year() as i16, date.month() as i8, date.day() as i8).ok()
    }

    fn time_from_chrono(time: chrono::NaiveTime) -> Option<civil::Time> {
        civil::Time::new(
            time.hour() as i8,
            time.minute() as i8,
            time.second() as i8,
            time.nanosecond() as i32,
        )
        .ok()
    }

    fn datetime_from_chrono(dt: chrono::NaiveDateTime) -> Option<civil::DateTime> {
        civil::DateTime::new(
            dt.year() as i16,
            dt.month() as i8,
            dt.day() as i8,
            dt.hour() as i8,
            dt.minute() as i8,
            dt.second() as i8,
            dt.nanosecond() as i32,
        )
        .ok()
    }

    impl<'a> FromSql<'a> for civil::Date {
        fn from_sql(val: &'a ColumnValues) -> Option<Self> {
            date_from_chrono(chrono::NaiveDate::from_sql(val)?)
        }
    }

    impl<'a> FromSql<'a> for civil::Time {
        fn from_sql(val: &'a ColumnValues) -> Option<Self> {
            time_from_chrono(chrono::NaiveTime::from_sql(val)?)
        }
    }

    impl<'a> FromSql<'a> for civil::DateTime {
        fn from_sql(val: &'a ColumnValues) -> Option<Self> {
            datetime_from_chrono(chrono::NaiveDateTime::from_sql(val)?)
        }
    }

    impl<'a> FromSql<'a> for Timestamp {
        fn from_sql(val: &'a ColumnValues) -> Option<Self> {
            let dt = chrono::DateTime::<chrono::FixedOffset>::from_sql(val)?;
            Timestamp::new(dt.timestamp(), dt.timestamp_subsec_nanos() as i32).ok()
        }
    }

    impl<'a> FromSql<'a> for Zoned {
        fn from_sql(val: &'a ColumnValues) -> Option<Self> {
            let dt = chrono::DateTime::<chrono::FixedOffset>::from_sql(val)?;
            let timestamp =
                Timestamp::new(dt.timestamp(), dt.timestamp_subsec_nanos() as i32).ok()?;
            let offset = Offset::from_seconds(dt.offset().local_minus_utc()).ok()?;
            Some(timestamp.to_zoned(offset.to_time_zone()))
        }
    }
}

// ---------------------------------------------------------------------------
// Decimal
// ---------------------------------------------------------------------------

impl<'a> FromSql<'a> for rust_decimal::Decimal {
    fn from_sql(val: &'a ColumnValues) -> Option<Self> {
        match val {
            ColumnValues::Decimal(d) | ColumnValues::Numeric(d) => {
                // DecimalParts has Display impl; parse through string for safety
                d.to_string().parse::<rust_decimal::Decimal>().ok()
            }
            ColumnValues::SmallMoney(m) => Some(rust_decimal::Decimal::new(m.int_val as i64, 4)),
            ColumnValues::Money(m) => {
                let low_bits = u32::from_ne_bytes(m.lsb_part.to_ne_bytes());
                let raw = (i64::from(m.msb_part) << 32) | i64::from(low_bits);
                Some(rust_decimal::Decimal::new(raw, 4))
            }
            _ => None,
        }
    }
}

/// A [`RowWriter`] that builds a [`Row`] directly during TDS wire decode,
/// avoiding the intermediate `Vec<ColumnValues>` allocation and the second
/// pass over values to decode strings.
pub(crate) struct DriverRowWriter {
    schema: Arc<RowSchema>,
    result_index: usize,
    values: Vec<ColumnValues>,
    decoded_strings: Vec<Option<String>>,
    col_count: usize,
}

impl DriverRowWriter {
    #[cfg(test)]
    pub(crate) fn new(schema: Arc<RowSchema>) -> Self {
        Self::with_result_index(schema, 0)
    }

    pub(crate) fn with_result_index(schema: Arc<RowSchema>, result_index: usize) -> Self {
        let col_count = schema.columns.len();
        Self {
            schema,
            result_index,
            values: Vec::with_capacity(col_count),
            decoded_strings: Vec::with_capacity(col_count),
            col_count,
        }
    }

    /// Takes the completed [`Row`], resetting the writer for the next row.
    pub(crate) fn take_row(&mut self) -> Row {
        let values = std::mem::replace(&mut self.values, Vec::with_capacity(self.col_count));
        let decoded_strings = std::mem::replace(
            &mut self.decoded_strings,
            Vec::with_capacity(self.col_count),
        );
        Row::from_parts(
            self.schema.clone(),
            values,
            self.result_index,
            decoded_strings,
        )
    }
}

impl RowWriter for DriverRowWriter {
    fn write_null(&mut self, _col: usize) {
        self.values.push(ColumnValues::Null);
        self.decoded_strings.push(None);
    }

    fn write_bool(&mut self, _col: usize, val: bool) {
        self.values.push(ColumnValues::Bit(val));
        self.decoded_strings.push(None);
    }

    fn write_u8(&mut self, _col: usize, val: u8) {
        self.values.push(ColumnValues::TinyInt(val));
        self.decoded_strings.push(None);
    }

    fn write_i16(&mut self, _col: usize, val: i16) {
        self.values.push(ColumnValues::SmallInt(val));
        self.decoded_strings.push(None);
    }

    fn write_i32(&mut self, _col: usize, val: i32) {
        self.values.push(ColumnValues::Int(val));
        self.decoded_strings.push(None);
    }

    fn write_i64(&mut self, _col: usize, val: i64) {
        self.values.push(ColumnValues::BigInt(val));
        self.decoded_strings.push(None);
    }

    fn write_f32(&mut self, _col: usize, val: f32) {
        self.values.push(ColumnValues::Real(val));
        self.decoded_strings.push(None);
    }

    fn write_f64(&mut self, _col: usize, val: f64) {
        self.values.push(ColumnValues::Float(val));
        self.decoded_strings.push(None);
    }

    fn write_string(&mut self, _col: usize, val: Cow<'_, [u8]>, encoding: EncodingType) {
        let val = SqlString::new(val.into_owned(), encoding);
        let decoded = val.to_utf8_string();
        self.values.push(ColumnValues::String(val));
        self.decoded_strings.push(Some(decoded));
    }

    fn write_bytes(&mut self, _col: usize, val: Cow<'_, [u8]>) {
        self.values.push(ColumnValues::Bytes(val.into_owned()));
        self.decoded_strings.push(None);
    }

    fn write_decimal(&mut self, _col: usize, val: DecimalParts) {
        self.values.push(ColumnValues::Decimal(val));
        self.decoded_strings.push(None);
    }

    fn write_numeric(&mut self, _col: usize, val: DecimalParts) {
        self.values.push(ColumnValues::Numeric(val));
        self.decoded_strings.push(None);
    }

    fn write_date(&mut self, _col: usize, val: SqlDate) {
        self.values.push(ColumnValues::Date(val));
        self.decoded_strings.push(None);
    }

    fn write_time(&mut self, _col: usize, val: SqlTime) {
        self.values.push(ColumnValues::Time(val));
        self.decoded_strings.push(None);
    }

    fn write_datetime(&mut self, _col: usize, val: SqlDateTime) {
        self.values.push(ColumnValues::DateTime(val));
        self.decoded_strings.push(None);
    }

    fn write_smalldatetime(&mut self, _col: usize, val: SqlSmallDateTime) {
        self.values.push(ColumnValues::SmallDateTime(val));
        self.decoded_strings.push(None);
    }

    fn write_datetime2(&mut self, _col: usize, val: SqlDateTime2) {
        self.values.push(ColumnValues::DateTime2(val));
        self.decoded_strings.push(None);
    }

    fn write_datetimeoffset(&mut self, _col: usize, val: SqlDateTimeOffset) {
        self.values.push(ColumnValues::DateTimeOffset(val));
        self.decoded_strings.push(None);
    }

    fn write_money(&mut self, _col: usize, val: SqlMoney) {
        self.values.push(ColumnValues::Money(val));
        self.decoded_strings.push(None);
    }

    fn write_smallmoney(&mut self, _col: usize, val: SqlSmallMoney) {
        self.values.push(ColumnValues::SmallMoney(val));
        self.decoded_strings.push(None);
    }

    fn write_uuid(&mut self, _col: usize, val: Uuid) {
        self.values.push(ColumnValues::Uuid(val));
        self.decoded_strings.push(None);
    }

    fn write_xml(&mut self, _col: usize, val: SqlXml) {
        let decoded = val.as_string();
        self.values.push(ColumnValues::Xml(val));
        self.decoded_strings.push(Some(decoded));
    }

    fn write_json(&mut self, _col: usize, val: SqlJson) {
        let decoded = val.as_string();
        self.values.push(ColumnValues::Json(val));
        self.decoded_strings.push(Some(decoded));
    }

    fn write_vector(&mut self, _col: usize, val: SqlVector) {
        self.values.push(ColumnValues::Vector(val));
        self.decoded_strings.push(None);
    }

    fn end_row(&mut self) {
        // No-op — row is taken via take_row().
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mssql_tds::datatypes::column_values::ColumnValues;
    use mssql_tds::datatypes::sql_string::SqlString;

    type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

    fn ensure_equal<T: PartialEq + std::fmt::Debug>(actual: T, expected: T) -> TestResult {
        if actual != expected {
            return Err(format!("expected {expected:?}, got {actual:?}").into());
        }
        Ok(())
    }

    // Helper to build a Row without real metadata
    fn make_row(names: &[&str], values: Vec<ColumnValues>) -> Row {
        make_typed_row(
            &names
                .iter()
                .map(|name| (*name, crate::column::ColumnType::Null))
                .collect::<Vec<_>>(),
            values,
        )
    }

    fn make_typed_row(
        columns: &[(&str, crate::column::ColumnType)],
        values: Vec<ColumnValues>,
    ) -> Row {
        let columns: Vec<Column> = columns
            .iter()
            .map(|(name, column_type)| Column::test_column(name, *column_type, 0))
            .collect();
        let name_map: HashMap<String, usize> = columns
            .iter()
            .enumerate()
            .map(|(i, c)| (c.name.clone(), i))
            .collect();
        let schema = Arc::new(RowSchema { columns, name_map });
        Row::from_schema(schema, values)
    }

    #[test]
    fn row_iteration_preserves_order_nulls_and_native_access() -> TestResult {
        use crate::compat::{FromSql as CompatFromSql, FromSqlOwned};

        let row = make_typed_row(
            &[
                ("first", crate::column::ColumnType::Int4),
                ("nullable", crate::column::ColumnType::Int4),
                ("last", crate::column::ColumnType::NVarchar),
            ],
            vec![
                ColumnValues::Int(7),
                ColumnValues::Null,
                ColumnValues::String(SqlString::from_utf8_string("last".into())),
            ],
        );
        ensure_equal(row.compat_values.get().is_none(), true)?;
        ensure_equal(row.get::<i32, _>("first"), Some(7))?;
        ensure_equal(row.compat_values.get().is_none(), true)?;

        let mut cells = row.cells();
        ensure_equal(row.compat_values.get().is_some(), true)?;
        let (first_column, first_value) = cells.next().ok_or("missing first cell")?;
        ensure_equal(first_column.name(), "first")?;
        ensure_equal(<i32 as CompatFromSql>::from_sql(first_value)?, Some(7))?;
        let (null_column, null_value) = cells.next().ok_or("missing NULL cell")?;
        ensure_equal(null_column.name(), "nullable")?;
        ensure_equal(<i32 as CompatFromSql>::from_sql(null_value)?, None)?;
        let (last_column, last_value) = cells.next().ok_or("missing last cell")?;
        ensure_equal(last_column.name(), "last")?;
        let last = <String as CompatFromSql>::from_sql(last_value)?;
        ensure_equal(last, Some("last".into()))?;
        ensure_equal(cells.next().is_none(), true)?;

        let values = row.clone().into_iter().collect::<Vec<_>>();
        ensure_equal(values.len(), 3)?;
        let first = i32::from_sql_owned(values.first().ok_or("missing owned first")?.clone())?;
        ensure_equal(first, Some(7))?;
        let null = i32::from_sql_owned(values.get(1).ok_or("missing owned NULL")?.clone())?;
        ensure_equal(null, None)?;
        let last = String::from_sql_owned(values.get(2).ok_or("missing owned last")?.clone())?;
        ensure_equal(last, Some("last".into()))?;

        ensure_equal(row.get::<i32, _>(0usize), Some(7))?;
        ensure_equal(row.get::<i32, _>("first"), Some(7))?;
        ensure_equal(row.get::<Option<i32>, _>("nullable"), Some(None))?;
        ensure_equal(row.get_compat::<i32, _>("first"), Some(7))?;
        ensure_equal(row.cells().count(), 3)?;
        ensure_equal(row.cells().count(), 3)?;
        ensure_equal(row.clone(), row.clone())?;

        let uncached = make_row(&["value"], vec![ColumnValues::Int(9)]);
        ensure_equal(uncached.compat_values.get().is_none(), true)?;
        let uncached_values = uncached.into_iter().collect::<Vec<_>>();
        ensure_equal(
            i32::from_sql_owned(
                uncached_values
                    .into_iter()
                    .next()
                    .ok_or("missing uncached value")?,
            )?,
            Some(9),
        )?;

        let exact = make_row(
            &["smallint", "xml", "json", "string"],
            vec![
                ColumnValues::SmallInt(7),
                ColumnValues::Xml(SqlXml::from("<root/>".to_string())),
                ColumnValues::Json(SqlJson::from("{\"ok\":true}".to_string())),
                ColumnValues::String(SqlString::from_utf8_string("text".into())),
            ],
        );
        ensure_equal(
            matches!(
                exact.try_get_compat::<i32, _>("smallint"),
                Err(Error::Conversion(_))
            ),
            true,
        )?;
        ensure_equal(
            matches!(
                exact.try_get_compat::<&str, _>("xml"),
                Err(Error::Conversion(_))
            ),
            true,
        )?;
        ensure_equal(
            matches!(
                exact.try_get_compat::<&str, _>("json"),
                Err(Error::Conversion(_))
            ),
            true,
        )?;
        ensure_equal(exact.try_get_compat::<&str, _>("string")?, Some("text"))?;
        Ok(())
    }

    #[test]
    fn null_bigbinary_uses_binary_compatibility_type() -> TestResult {
        use mssql_tds::datatypes::sqldatatypes::{TdsDataType, TypeInfo};
        use mssql_tds::test_client_support::int_columns;

        let mut metadata = int_columns(1);
        let column = metadata.first_mut().ok_or("missing column")?;
        column.data_type = TdsDataType::BigBinary;
        column.type_info = TypeInfo::var_len(TdsDataType::BigBinary, 10)?;
        let row = Row::from_schema(
            RowSchema::from_metadata(&metadata),
            vec![ColumnValues::Null],
        );
        ensure_equal(row.try_get_compat::<&[u8], _>(0usize)?, None)?;
        ensure_equal(
            matches!(
                row.try_get_compat::<&str, _>(0usize),
                Err(Error::Conversion(_))
            ),
            true,
        )?;
        Ok(())
    }

    #[test]
    fn empty_row_iteration_is_empty() {
        let row = make_row(&[], Vec::new());
        assert_eq!(row.cells().count(), 0);
        assert_eq!(row.into_iter().count(), 0);
    }

    #[test]
    #[should_panic(expected = "column conversion failed: ColumnNotFound(\"missing\")")]
    fn get_compat_panic_includes_conversion_error() {
        make_row(&["present"], vec![ColumnValues::Int(1)]).get_compat::<i32, _>("missing");
    }

    #[test]
    fn get_by_name() {
        let row = make_row(
            &["id", "name"],
            vec![
                ColumnValues::Int(42),
                ColumnValues::String(SqlString::from_utf8_string("hello".into())),
            ],
        );
        assert_eq!(row.get::<i32, _>("id"), Some(42));
        assert_eq!(row.get::<String, _>("name"), Some("hello".into()));
        // &str borrowing from pre-decoded cache
        assert_eq!(row.get::<&str, _>("name"), Some("hello"));
    }

    #[test]
    fn get_by_index() {
        let row = make_row(&["a"], vec![ColumnValues::BigInt(99)]);
        assert_eq!(row.get::<i64, _>(0usize), Some(99));
    }

    #[test]
    fn get_null() {
        let row = make_row(&["x"], vec![ColumnValues::Null]);
        assert_eq!(row.get::<Option<i32>, _>("x"), Some(None));
        assert_eq!(row.get::<i32, _>("x"), None);
    }

    #[test]
    fn try_get_missing_column() {
        let row = make_row(&["a"], vec![ColumnValues::Int(1)]);
        let error = row
            .try_get::<i32, _>("nope")
            .expect_err("a missing column must fail lookup");
        assert!(matches!(error, Error::ColumnNotFound(name) if name == "nope"));
    }

    #[test]
    fn schema_name_lookup_keeps_first_duplicate_and_unique_columns() {
        let mut metadata = mssql_tds::test_client_support::int_columns(3);
        for (column, name) in metadata
            .iter_mut()
            .zip(["duplicate", "unique", "duplicate"])
        {
            column.column_name = name.to_string();
        }
        let row = Row::from_tds(
            &metadata,
            vec![
                ColumnValues::Int(7),
                ColumnValues::Int(8),
                ColumnValues::Int(9),
            ],
        );

        assert_eq!(row.get::<i32, _>("duplicate"), Some(7));
        assert_eq!(row.get::<i32, _>("unique"), Some(8));
    }

    #[test]
    fn get_bool() {
        let row = make_row(&["b"], vec![ColumnValues::Bit(true)]);
        assert_eq!(row.get::<bool, _>("b"), Some(true));
    }

    #[test]
    fn get_uuid() {
        let u = uuid::Uuid::new_v4();
        let row = make_row(&["id"], vec![ColumnValues::Uuid(u)]);
        assert_eq!(row.get::<uuid::Uuid, _>("id"), Some(u));
    }

    #[test]
    fn widening_int_conversions() {
        let row = make_row(&["v"], vec![ColumnValues::TinyInt(7)]);
        assert_eq!(row.get::<i16, _>("v"), Some(7));
        assert_eq!(row.get::<i32, _>("v"), Some(7));
        assert_eq!(row.get::<i64, _>("v"), Some(7));
    }

    #[test]
    fn float_widening() {
        let row = make_row(&["v"], vec![ColumnValues::Real(1.5)]);
        assert_eq!(row.get::<f64, _>("v"), Some(1.5));
    }

    #[test]
    fn money_decimal_preserves_signed_high_and_unsigned_low_bits() {
        for (msb_part, lsb_part, raw) in [
            (0, -1, i64::from(u32::MAX)),
            (-1, -1, -1),
            (i32::MIN, 0, i64::MIN),
            (i32::MAX, -1, i64::MAX),
        ] {
            let value = ColumnValues::Money(SqlMoney { msb_part, lsb_part });
            assert_eq!(
                rust_decimal::Decimal::from_sql(&value),
                Some(rust_decimal::Decimal::new(raw, 4))
            );
        }
    }

    #[test]
    fn bytes_extraction() {
        let row = make_row(&["b"], vec![ColumnValues::Bytes(vec![1, 2, 3])]);
        assert_eq!(row.get::<Vec<u8>, _>("b"), Some(vec![1, 2, 3]));
        assert_eq!(row.get::<&[u8], _>("b"), Some(&[1, 2, 3][..]));
    }

    #[test]
    fn spatial_bytes_preserve_row_construction_and_writer_behavior() -> TestResult {
        use crate::ColumnType;
        use mssql_tds::test_client_support::udt_column_with_metadata;

        for (name, column_type, srid) in [
            ("geography", ColumnType::Geography, 4326u32),
            ("geometry", ColumnType::Geometry, 0u32),
        ] {
            let metadata = [udt_column_with_metadata(u16::MAX, "", "sys", name, "")];
            let mut bytes = srid.to_le_bytes().to_vec();
            bytes.extend_from_slice(&[1, 12]);
            bytes.extend_from_slice(&1.0f64.to_le_bytes());
            bytes.extend_from_slice(&2.0f64.to_le_bytes());
            let expected = Row::from_tds(&metadata, vec![ColumnValues::Bytes(bytes.clone())]);
            let mut writer = DriverRowWriter::new(RowSchema::from_metadata(&metadata));

            writer.write_null(0);
            let null = writer.take_row();
            ensure_equal(
                null.columns()
                    .first()
                    .ok_or("missing null column")?
                    .column_type(),
                column_type,
            )?;
            ensure_equal(null.get::<Vec<u8>, _>(0usize), None)?;
            ensure_equal(null.get::<Option<&[u8]>, _>(0usize), Some(None))?;
            ensure_equal(null.raw_value(0), Some(&ColumnValues::Null))?;

            writer.write_bytes(0, Cow::Borrowed(&bytes));
            let row = writer.take_row();
            ensure_equal(&row, &expected)?;
            ensure_equal(
                row.columns()
                    .first()
                    .ok_or("missing spatial column")?
                    .column_type(),
                column_type,
            )?;
            ensure_equal(row.get::<Vec<u8>, _>("udt"), Some(bytes.clone()))?;
            ensure_equal(row.try_get::<&[u8], _>(0usize)?, Some(bytes.as_slice()))?;
            let Some(ColumnValues::Bytes(raw)) = row.raw_value(0) else {
                return Err("spatial values must retain the upstream Bytes representation".into());
            };
            ensure_equal(
                row.get::<&[u8], _>("udt")
                    .ok_or("missing spatial bytes")?
                    .as_ptr(),
                raw.as_ptr(),
            )?;
            ensure_equal(row.get::<String, _>("udt"), None)?;
            ensure_equal(row.get::<&str, _>("udt"), None)?;

            writer.write_bytes(0, Cow::Owned(Vec::new()));
            let empty = writer.take_row();
            ensure_equal(empty.get::<Vec<u8>, _>("udt"), Some(Vec::new()))?;
            if empty == null {
                return Err("empty spatial bytes must differ from SQL NULL".into());
            }
            if row != expected {
                return Err("reusing the writer must not alter earlier rows".into());
            }
        }
        Ok(())
    }

    #[test]
    fn writer_owns_borrowed_wire_values() {
        let schema = make_row(&["text", "bytes"], vec![]).schema;
        let mut writer = DriverRowWriter::new(schema);
        let mut text: Vec<u8> = "héllo".encode_utf16().flat_map(u16::to_le_bytes).collect();
        let mut bytes = vec![1, 2, 3];
        writer.write_string(0, Cow::Borrowed(&text), EncodingType::Utf16);
        writer.write_bytes(1, Cow::Borrowed(&bytes));
        let first = writer.take_row();
        text.fill(0);
        bytes.fill(0);

        writer.write_string(0, Cow::Owned(b"next".to_vec()), EncodingType::Utf8);
        writer.write_bytes(1, Cow::Owned(vec![4, 5]));
        let second = writer.take_row();

        assert_eq!(first.get::<&str, _>("text"), Some("héllo"));
        assert_eq!(first.get::<&[u8], _>("bytes"), Some(&[1, 2, 3][..]));
        assert_eq!(second.get::<&str, _>("text"), Some("next"));
        assert_eq!(second.get::<&[u8], _>("bytes"), Some(&[4, 5][..]));
    }

    #[test]
    fn case_insensitive_lookup() -> TestResult {
        let row = make_row(
            &["UserName", "id"],
            vec![
                ColumnValues::String(SqlString::from_utf8_string("ada".into())),
                ColumnValues::Int(7),
            ],
        );

        ensure_equal(row.try_get_ci::<&str>("username")?, Some("ada"))?;
        ensure_equal(row.get_ci::<String>("USERNAME"), Some("ada".into()))?;
        ensure_equal(row.try_get_ci::<i32>("ID")?, Some(7))?;
        if !matches!(
            row.try_get_ci::<i32>("missing"),
            Err(Error::ColumnNotFound(name)) if name == "missing"
        ) {
            return Err("expected ColumnNotFound for missing column".into());
        }
        Ok(())
    }

    #[test]
    fn try_get_errors_without_panicking() -> TestResult {
        let row = make_row(
            &["a"],
            vec![ColumnValues::String(SqlString::from_utf8_string(
                "x".into(),
            ))],
        );

        if !matches!(
            row.try_get::<i32, _>(1usize),
            Err(Error::ColumnIndexOutOfBounds { index: 1, count: 1 })
        ) {
            return Err("expected ColumnIndexOutOfBounds for index 1".into());
        }
        if !matches!(
            row.try_get::<i32, _>("missing"),
            Err(Error::ColumnNotFound(name)) if name == "missing"
        ) {
            return Err("expected ColumnNotFound for missing column".into());
        }
        ensure_equal(row.try_get::<i32, _>("a")?, None)?;
        Ok(())
    }

    #[test]
    fn null_smallint_as_i32_returns_none_cleanly() -> TestResult {
        let columns = vec![Column::test_column(
            "small",
            crate::column::ColumnType::Int2,
            2,
        )];
        let name_map = HashMap::from([("small".to_string(), 0)]);
        let schema = Arc::new(RowSchema { columns, name_map });
        let row = Row::from_schema(schema, vec![ColumnValues::Null]);

        ensure_equal(row.try_get::<i32, _>("small")?, None)?;
        ensure_equal(row.try_get::<Option<i32>, _>("small")?, Some(None))?;
        Ok(())
    }

    #[test]
    fn optional_conversion_distinguishes_null_from_incompatible_values() {
        let row = make_row(
            &["null", "number", "text"],
            vec![
                ColumnValues::Null,
                ColumnValues::Int(42),
                ColumnValues::String(SqlString::from_utf8_string("text".into())),
            ],
        );
        assert_eq!(row.get::<Option<i32>, _>("null"), Some(None));
        assert_eq!(row.get::<Option<i32>, _>("number"), Some(Some(42)));
        assert_eq!(row.get::<Option<i32>, _>("text"), None);
        assert_eq!(Option::<i32>::from_sql(&ColumnValues::Null), Some(None));
        assert_eq!(
            Option::<i32>::from_sql(&ColumnValues::Int(42)),
            Some(Some(42))
        );
        assert_eq!(Option::<i32>::from_sql(&ColumnValues::Bit(true)), None);
    }

    #[test]
    fn legacy_datetime_preserves_fractional_ticks() {
        use mssql_tds::datatypes::column_values::SqlDateTime;
        let base = chrono::NaiveDate::from_ymd_opt(1900, 1, 1)
            .expect("valid date")
            .and_hms_opt(0, 0, 0)
            .expect("valid time");
        for (ticks, nanos) in [
            (0, 0),
            (1, 3_333_333),
            (2, 6_666_667),
            (299, 996_666_667),
            (300, 1_000_000_000),
            (25_919_999, 86_399_996_666_667),
        ] {
            let value = ColumnValues::DateTime(SqlDateTime {
                days: -1,
                time: ticks,
            });
            let expected = base - chrono::Duration::days(1) + chrono::Duration::nanoseconds(nanos);
            assert_eq!(chrono::NaiveDateTime::from_sql(&value), Some(expected));
        }
    }

    #[test]
    fn str_borrowing() {
        let row = make_row(
            &["s", "n"],
            vec![
                ColumnValues::String(SqlString::from_utf8_string("borrowed".into())),
                ColumnValues::Null,
            ],
        );
        // Can borrow &str from the row
        assert_eq!(row.get::<&str, _>("s"), Some("borrowed"));
        // NULL returns None
        assert_eq!(row.get::<&str, _>("n"), None);
        // Option<&str> works too
        assert_eq!(row.get::<Option<&str>, _>("s"), Some(Some("borrowed")));
        assert_eq!(row.get::<Option<&str>, _>("n"), Some(None));
    }

    #[test]
    fn nvarchar_utf16_replaces_only_malformed_sequences() {
        let cases: &[(&str, &[u8], &str)] = &[
            ("lone high surrogate", &[0x00, 0xD8], "\u{FFFD}"),
            ("lone low surrogate", &[0x00, 0xDC], "\u{FFFD}"),
            ("odd byte length", &[0x41, 0x00, 0x42], "A\u{FFFD}"),
            (
                "surrogate between valid characters",
                &[0x41, 0x00, 0x00, 0xD8, 0x42, 0x00],
                "A\u{FFFD}B",
            ),
            (
                "reversed surrogate pair",
                &[0x00, 0xDC, 0x00, 0xD8],
                "\u{FFFD}\u{FFFD}",
            ),
            (
                "valid surrogate pair",
                &[0x41, 0x00, 0x3D, 0xD8, 0x00, 0xDE, 0x42, 0x00],
                "A\u{1F600}B",
            ),
        ];

        for &(name, bytes, expected) in cases {
            let row = make_row(
                &["s"],
                vec![ColumnValues::String(SqlString::new(
                    bytes.to_vec(),
                    EncodingType::Utf16,
                ))],
            );

            assert_eq!(
                row.get::<String, _>(0usize).as_deref(),
                Some(expected),
                "{name}: owned string",
            );
            assert_eq!(
                row.get::<&str, _>("s"),
                Some(expected),
                "{name}: borrowed string",
            );
        }
    }

    #[test]
    fn str_non_string_column_returns_none() {
        let row = make_row(&["i"], vec![ColumnValues::Int(42)]);
        assert_eq!(row.get::<&str, _>("i"), None);
    }

    // ── PartialEq for Row (issue #65) ──

    #[test]
    fn eq_same_arc_schema_same_values() {
        let columns = vec![Column::test_column(
            "id",
            crate::column::ColumnType::Int4,
            4,
        )];
        let name_map: HashMap<String, usize> = [("id".to_string(), 0)].into();
        let schema = Arc::new(RowSchema { columns, name_map });

        let a = Row::from_schema(Arc::clone(&schema), vec![ColumnValues::Int(1)]);
        let b = Row::from_schema(schema, vec![ColumnValues::Int(1)]);
        assert_eq!(a, b);
    }

    #[test]
    fn eq_independent_schemas_with_matching_types_and_values() {
        let r1 = make_row(
            &["id", "name"],
            vec![
                ColumnValues::Int(42),
                ColumnValues::String(SqlString::from_utf8_string("hi".into())),
            ],
        );
        let r2 = make_row(
            &["different_id", "different_name"],
            vec![
                ColumnValues::Int(42),
                ColumnValues::String(SqlString::from_utf8_string("hi".into())),
            ],
        );
        assert_eq!(r1, r2, "names should not affect equality");
    }

    #[test]
    fn ne_different_values() {
        let a = make_row(&["v"], vec![ColumnValues::Int(1)]);
        let b = make_row(&["v"], vec![ColumnValues::Int(2)]);
        assert_ne!(a, b);
    }

    #[test]
    fn ne_different_arity() {
        let a = make_row(&["v"], vec![ColumnValues::Int(1)]);
        let b = make_row(
            &["a", "b"],
            vec![ColumnValues::Int(1), ColumnValues::Int(2)],
        );
        assert_ne!(a, b);
    }

    #[test]
    fn ne_different_column_types_same_arity() {
        // Build two rows where the value types coincide on the wire (both
        // are the driver's "Int" carrier) but the schemas claim different
        // column types — equality should reject the mismatch.
        let cols_a = vec![Column::test_column("v", crate::column::ColumnType::Int4, 4)];
        let cols_b = vec![Column::test_column("v", crate::column::ColumnType::Int8, 8)];
        let schema_a = Arc::new(RowSchema {
            name_map: [("v".to_string(), 0)].into(),
            columns: cols_a,
        });
        let schema_b = Arc::new(RowSchema {
            name_map: [("v".to_string(), 0)].into(),
            columns: cols_b,
        });
        let a = Row::from_schema(schema_a, vec![ColumnValues::Int(1)]);
        let b = Row::from_schema(schema_b, vec![ColumnValues::Int(1)]);
        assert_ne!(a, b);
    }

    #[test]
    fn ne_nan_floats() {
        // PartialEq follows IEEE-754 — NaN is never equal to itself.
        let a = make_row(&["v"], vec![ColumnValues::Float(f64::NAN)]);
        let b = make_row(&["v"], vec![ColumnValues::Float(f64::NAN)]);
        assert_ne!(a, b);
    }

    #[test]
    fn eq_nulls() {
        let a = make_row(&["x"], vec![ColumnValues::Null]);
        let b = make_row(&["x"], vec![ColumnValues::Null]);
        assert_eq!(a, b);
    }
}
