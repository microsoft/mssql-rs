// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Python bulk copy adapter for streaming rows into TDS bulk load.
//!
//! This module provides adapters that bridge Python data to the `BulkLoadRow`
//! trait. Each row is materialized once into a temporary `Vec<ColumnValues>`
//! inside a `Python::attach` scope and then written to the TDS packet; rows
//! are not buffered at the batch level. The current caller,
//! `Cursor::bulkcopy`, holds the GIL for the whole operation.

use std::sync::Arc;

use crate::types::py_to_column_value;
use async_trait::async_trait;
use chrono::{Datelike, NaiveDate, NaiveTime, TimeDelta, Timelike};
use mssql_tds::connection::bulk_copy::{BulkLoadRow, ResolvedColumnMapping};
use mssql_tds::core::TdsResult;
use mssql_tds::datatypes::bulk_copy_metadata::{BulkCopyColumnMetadata, SqlDbType};
use mssql_tds::datatypes::column_values::{ColumnValues, SqlDateTime2, SqlDateTimeOffset, SqlTime};
use mssql_tds::datatypes::decoder::DecimalParts;
use mssql_tds::datatypes::sql_json::SqlJson;
use mssql_tds::datatypes::sql_vector::SqlVector;
use mssql_tds::datatypes::sqldatatypes::VectorBaseType;
use mssql_tds::error::Error;
use mssql_tds::message::bulk_load::StreamingBulkLoadWriter;
use pyo3::prelude::*;
use pyo3::types::{PyDate, PyDateTime, PyTime};

use pyo3::types::{PyString, PyTuple};
use rust_decimal::Decimal;
use rust_decimal::prelude::*;
use tracing::instrument;

fn parse_iso_time(value: &str) -> Option<NaiveTime> {
    let (hour, minute, second, nanoseconds) = parse_iso_time_components(value)?;
    NaiveTime::from_hms_nano_opt(hour, minute, second, nanoseconds)
}

fn parse_iso_time_components(value: &str) -> Option<(u32, u32, u32, u32)> {
    if !value.is_ascii() {
        return None;
    }
    let (mut clock, mut fraction) = value
        .split_once(['.', ','])
        .map_or((value, None), |(clock, fraction)| (clock, Some(fraction)));
    // Python also accepts HHMMSSff and HH:MM:SS:f without a decimal separator.
    if fraction.is_none() {
        if clock.as_bytes().get(8) == Some(&b':') {
            fraction = Some(&clock[9..]);
            clock = &clock[..8];
        } else if clock.len() >= 8 && !clock.contains(':') {
            fraction = Some(&clock[6..]);
            clock = &clock[..6];
        }
    }
    let nanoseconds = match fraction {
        Some(fraction)
            if !fraction.is_empty()
                && fraction.len() <= 7
                && fraction.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            fraction.parse::<u32>().ok()? * 10u32.pow(9 - fraction.len() as u32)
        }
        Some(_) => return None,
        None => 0,
    };
    let (hour, minute, second) = if clock.contains(':') {
        let mut parts = clock.split(':');
        let hour = parts.next()?;
        let minute = parts.next()?;
        let second = parts.next().unwrap_or("00");
        if parts.next().is_some() {
            return None;
        }
        (hour, minute, second)
    } else {
        match clock.len() {
            2 => (clock, "00", "00"),
            4 => (&clock[..2], &clock[2..], "00"),
            6 => (&clock[..2], &clock[2..4], &clock[4..]),
            _ => return None,
        }
    };
    let component = |part: &str| {
        (part.len() == 2 && part.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| part.parse::<u32>().ok())
            .flatten()
    };
    Some((
        component(hour)?,
        component(minute)?,
        component(second)?,
        nanoseconds,
    ))
}

fn parse_bulk_datetime(value: &str, metadata: &BulkCopyColumnMetadata) -> TdsResult<ColumnValues> {
    let invalid = || {
        Error::UsageError(format!(
            "Invalid ISO datetime '{value}' for column '{}': expected a valid date and time with at most seven fractional digits",
            metadata.column_name
        ))
    };
    let (date, time, zone) = parse_iso_datetime(value).ok_or_else(invalid)?;
    if !(1..=9999).contains(&date.year()) {
        return Err(invalid());
    }

    let offset = match zone {
        "" | "Z" => 0,
        _ if zone.starts_with(['+', '-']) => {
            let (hour, minute, second, nanoseconds) =
                parse_iso_time_components(&zone[1..]).ok_or_else(invalid)?;
            // Unlike clock fields, Python offset fields carry into the next unit.
            let seconds = i64::from(hour * 3600 + minute * 60 + second);
            if seconds >= 24 * 3600 {
                return Err(invalid());
            }
            if metadata.sql_type == SqlDbType::DateTimeOffset
                && (seconds % 60 != 0 || nanoseconds != 0)
            {
                return Err(Error::UsageError(
                    "DATETIMEOFFSET requires a whole-minute UTC offset".into(),
                ));
            }
            if zone.starts_with('-') {
                -seconds
            } else {
                seconds
            }
        }
        _ => return Err(invalid()),
    };
    let mut datetime = date.and_time(time);
    if metadata.sql_type == SqlDbType::DateTimeOffset {
        if !(-840 * 60..=840 * 60).contains(&offset) {
            return Err(Error::UsageError(
                "Timezone offset is outside the DATETIMEOFFSET range (-14:00 to +14:00)".into(),
            ));
        }
        datetime = datetime
            .checked_sub_signed(TimeDelta::seconds(offset))
            .filter(|datetime| (1..=9999).contains(&datetime.year()))
            .ok_or_else(invalid)?;
    }
    let datetime2 = SqlDateTime2 {
        days: (datetime.num_days_from_ce() - 1) as u32,
        time: SqlTime {
            time_nanoseconds: u64::from(datetime.num_seconds_from_midnight()) * 10_000_000
                + u64::from(datetime.nanosecond()) / 100,
            scale: metadata.scale,
        },
    };
    if metadata.sql_type == SqlDbType::DateTimeOffset {
        Ok(ColumnValues::DateTimeOffset(SqlDateTimeOffset {
            datetime2,
            offset: (offset / 60) as i16,
        }))
    } else {
        Ok(ColumnValues::DateTime2(datetime2))
    }
}

fn parse_iso_datetime(value: &str) -> Option<(NaiveDate, NaiveTime, &str)> {
    fn with_time((date, remainder): (NaiveDate, &str)) -> Option<(NaiveDate, NaiveTime, &str)> {
        if remainder.is_empty() {
            return Some((date, NaiveTime::MIN, ""));
        }
        let clock = &remainder[remainder.chars().next()?.len_utf8()..];
        let offset_start = clock.find(['+', '-', 'Z']);
        let (clock, zone) = offset_start.map_or((clock, ""), |index| clock.split_at(index));
        Some((date, parse_iso_time(clock)?, zone))
    }
    let bytes = value.as_bytes();
    let week_boundary = if bytes.get(4..6) == Some(b"-W") {
        // Match CPython's boundary choice, not whichever date/time split parses first.
        // In YYYY-Www-12..., '-' separates the clock unless position 10 is nonnumeric.
        Some(
            if bytes.get(8) == Some(&b'-') && !bytes.get(10).is_some_and(u8::is_ascii_digit) {
                (10, "%G-W%V-%u", false)
            } else {
                (8, "%G-W%V", true)
            },
        )
    } else if bytes.get(4) == Some(&b'W') {
        let digit_end = 7 + bytes
            .get(7..)?
            .iter()
            .take_while(|b| b.is_ascii_digit())
            .count();
        let boundary = if digit_end < 9 {
            digit_end
        } else if digit_end % 2 == 0 {
            7
        } else {
            8
        };
        Some((
            boundary,
            if boundary == 7 { "%GW%V" } else { "%GW%V%u" },
            boundary == 7,
        ))
    } else {
        None
    };
    if let Some((boundary, format, monday)) = week_boundary {
        let mut parsed = chrono::format::Parsed::new();
        chrono::format::parse(
            &mut parsed,
            value.get(..boundary)?,
            chrono::format::StrftimeItems::new(format),
        )
        .ok()?;
        if monday {
            parsed.set_weekday(chrono::Weekday::Mon).ok()?;
        }
        return with_time((parsed.to_naive_date().ok()?, value.get(boundary..)?));
    }
    ["%Y-%m-%d", "%Y%m%d"].iter().find_map(|format| {
        NaiveDate::parse_and_remainder(value, format)
            .ok()
            .and_then(with_time)
    })
}

/// Represents the source Python type for conversion mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourcePythonType {
    None,
    String,
    Int,
    Float,
    Bytes,
    Bool,
    Decimal,
    Date,
    DateTime,
    Time,
    Uuid,
    Dict,
    List,
    Other,
}

impl SourcePythonType {
    /// Detect the Python type from a PyAny object (fast type checking).
    #[inline]
    fn detect(py_obj: &Bound<'_, PyAny>) -> Self {
        if py_obj.is_none() {
            SourcePythonType::None
        } else if py_obj.is_instance_of::<PyString>() {
            SourcePythonType::String
        } else if py_obj.is_instance_of::<pyo3::types::PyInt>() {
            SourcePythonType::Int
        } else if py_obj.is_instance_of::<pyo3::types::PyFloat>() {
            SourcePythonType::Float
        } else if py_obj.is_instance_of::<pyo3::types::PyBytes>() {
            SourcePythonType::Bytes
        } else if py_obj.is_instance_of::<pyo3::types::PyBool>() {
            SourcePythonType::Bool
        } else if py_obj.is_instance_of::<pyo3::types::PyDict>() {
            SourcePythonType::Dict
        } else if py_obj.is_instance_of::<pyo3::types::PyList>() {
            SourcePythonType::List
        } else if py_obj.is_instance_of::<PyDateTime>() {
            // Check for datetime before date, since datetime is a subclass of date
            SourcePythonType::DateTime
        } else if py_obj.is_instance_of::<PyDate>() {
            SourcePythonType::Date
        } else if py_obj.is_instance_of::<PyTime>() {
            SourcePythonType::Time
        } else {
            let py = py_obj.py();
            // Check for decimal.Decimal
            if let Ok(decimal_module) = pyo3::types::PyModule::import(py, "decimal")
                && let Ok(decimal_class) = decimal_module.getattr("Decimal")
                && let Ok(is_instance) = py_obj.is_instance(&decimal_class)
                && is_instance
            {
                return SourcePythonType::Decimal;
            }
            // Check for uuid.UUID
            if let Ok(uuid_module) = pyo3::types::PyModule::import(py, "uuid")
                && let Ok(uuid_class) = uuid_module.getattr("UUID")
                && let Ok(is_instance) = py_obj.is_instance(&uuid_class)
                && is_instance
            {
                return SourcePythonType::Uuid;
            }
            SourcePythonType::Other
        }
    }
}

/// Adapter that wraps a Python tuple for bulk insert.
///
/// This struct implements the `BulkLoadRow` trait. Each row's Python values
/// are converted into one temporary `Vec<ColumnValues>` inside the adapter's
/// `Python::attach` scope, then written to the packet after that scope ends.
///
/// When destination metadata is provided, the adapter will attempt to perform
/// type conversions (e.g., string to int) to match the target column types.
pub struct PythonRowAdapter {
    /// Python tuple containing row data (stored as Py<PyAny> for Send + Sync)
    row: Py<PyAny>,
    /// Optional destination column metadata for type coercion (wrapped in Arc for efficient sharing across rows)
    destination_metadata: Option<Arc<Vec<BulkCopyColumnMetadata>>>,
    /// Optional resolved column mappings for reordering columns (wrapped in Arc for efficient sharing across rows)
    resolved_mappings: Option<Arc<Vec<ResolvedColumnMapping>>>,
    /// Expected number of source columns (from first row), used to validate row consistency
    expected_source_columns: Option<usize>,
}

impl PythonRowAdapter {
    /// Create a new Python row adapter with destination metadata for type coercion.
    ///
    /// # Arguments
    ///
    /// * `row` - Python tuple containing column values
    /// * `destination_metadata` - Destination column metadata for type conversion (wrapped in Arc for efficient sharing)
    /// * `resolved_mappings` - Optional resolved column mappings for reordering (wrapped in Arc for efficient sharing)
    ///
    /// # Returns
    ///
    /// A new PythonRowAdapter with type coercion support.
    pub fn with_metadata(
        row: Py<PyAny>,
        destination_metadata: Arc<Vec<BulkCopyColumnMetadata>>,
        resolved_mappings: Option<Arc<Vec<ResolvedColumnMapping>>>,
        expected_source_columns: Option<usize>,
    ) -> Self {
        Self {
            row,
            destination_metadata: Some(destination_metadata),
            resolved_mappings,
            expected_source_columns,
        }
    }

    /// Convert a Python value to a ColumnValue with type coercion based on target SQL type.
    ///
    /// This function implements a source-to-target type mapping strategy:
    /// 1. Detect the source Python type (fast type check)
    /// 2. Validate NULL handling against target column nullability
    /// 3. Check if coercion is needed based on (source_type, target_sql_type) pair
    /// 4. Apply appropriate conversion or fall back to default conversion
    ///
    /// # Type Coercion Matrix (extensible)
    ///
    /// | Source Python Type | Target SQL Type(s)              | Coercion Strategy           |
    /// |--------------------|----------------------------------|------------------------------|
    /// | None               | Any non-nullable                | Error: NULL constraint       |
    /// | None               | Any nullable                    | ColumnValues::Null           |
    /// | String             | INT/BIGINT/SMALLINT/TINYINT     | Parse string → integer       |
    /// | String             | (future: DECIMAL/NUMERIC)       | Parse string → decimal       |
    /// | String             | (future: DATETIME/DATE)         | Parse string → datetime      |
    /// | (default)          | Any                             | py_to_column_value()         |
    fn convert_with_coercion(
        py_obj: &Bound<'_, PyAny>,
        target_metadata: Option<&BulkCopyColumnMetadata>,
    ) -> TdsResult<ColumnValues> {
        // Step 1: Fast source type detection
        let source_type = SourcePythonType::detect(py_obj);

        // Step 2: Handle NULL values with validation
        if source_type == SourcePythonType::None {
            return Self::handle_null_value(target_metadata).map_err(|e| {
                tracing::error!("NULL validation failed: {}", e);
                e
            });
        }

        // Step 3: Check if we need type coercion based on source → target mapping
        if let Some(meta) = target_metadata
            && let Some(coerced_value) = Self::try_type_coercion(py_obj, source_type, meta)
                .map_err(|e| {
                    tracing::error!("Type coercion failed: {}", e);
                    e
                })?
        {
            return Ok(coerced_value);
        }

        // Step 4: Fall back to default Python → TDS conversion
        // ESSENTIAL: This line handles all native type conversions!
        // When try_type_coercion returns None (no special transformation needed),
        // we fall back to py_to_column_value to handle the conversion.
        //
        // Passes target_metadata for type validation.
        // This ensures that if try_type_coercion incorrectly returns None,
        // the type mismatch will be caught and reported explicitly rather
        // than silently producing wrong data.
        //
        // This handles:
        // - Python datetime.date → SQL DATE conversion
        // - Native type conversions (float→float, bytes→bytes, etc.)
        // - Bulk copy without explicit column metadata
        //
        py_to_column_value(py_obj, target_metadata)
    }

    /// Handle NULL value insertion with nullability validation.
    #[inline]
    fn handle_null_value(
        target_metadata: Option<&BulkCopyColumnMetadata>,
    ) -> TdsResult<ColumnValues> {
        if let Some(meta) = target_metadata
            && !meta.is_nullable
        {
            return Err(Error::UsageError(format!(
                "Cannot insert NULL value into non-nullable column '{}'. Conversion not possible for NULL to non-nullable column",
                meta.column_name
            )));
        }
        Ok(ColumnValues::Null)
    }

    /// Try type coercion based on source Python type and target SQL type.
    ///
    /// Returns Some(ColumnValues) if coercion was applied, None if no coercion needed.
    ///
    /// This method encapsulates the type coercion mapping logic, making it easy to add
    /// new conversions by pattern matching on (source_type, target_sql_type) pairs.
    fn try_type_coercion(
        py_obj: &Bound<'_, PyAny>,
        source_type: SourcePythonType,
        target_meta: &BulkCopyColumnMetadata,
    ) -> TdsResult<Option<ColumnValues>> {
        // Type coercion dispatch based on (source → target) mapping
        match (source_type, target_meta.sql_type) {
            // String → Integer types: Parse string as integer
            (
                SourcePythonType::String,
                SqlDbType::Int | SqlDbType::BigInt | SqlDbType::SmallInt | SqlDbType::TinyInt,
            ) => {
                let result = Self::coerce_string_to_integer(py_obj, target_meta.sql_type)?;
                Ok(Some(result))
            }

            // Int → Integer types: Validate range for target type
            // This ensures Python integers that exceed the target type's range are rejected
            // instead of being silently converted to a different integer type
            (
                SourcePythonType::Int,
                SqlDbType::Int | SqlDbType::BigInt | SqlDbType::SmallInt | SqlDbType::TinyInt,
            ) => {
                let result = Self::coerce_python_int_to_integer(py_obj, target_meta.sql_type)?;
                Ok(Some(result))
            }

            // Int → Bit: Convert Python integer to boolean
            // 0 = false, non-zero = true (matches SQL Server's implicit conversion)
            (SourcePythonType::Int, SqlDbType::Bit) => {
                let value = py_obj.extract::<i64>().map_err(|e| {
                    Error::UsageError(format!("Cannot extract Python integer: {}", e))
                })?;
                Ok(Some(ColumnValues::Bit(value != 0)))
            }

            // String → Bit: Parse string as boolean
            // Accepts: "0"/"1", "true"/"false", "True"/"False", "TRUE"/"FALSE"
            (SourcePythonType::String, SqlDbType::Bit) => {
                let py_str = py_obj
                    .cast::<PyString>()
                    .map_err(|e| Error::UsageError(format!("Failed to cast to string: {}", e)))?;

                let s = py_str
                    .to_str()
                    .map_err(|e| Error::UsageError(format!("Failed to extract string: {}", e)))?;

                let bit_value = match s.to_lowercase().as_str() {
                    "0" | "false" => false,
                    "1" | "true" => true,
                    _ => {
                        return Err(Error::UsageError(format!(
                            "Cannot convert string '{}' to BIT. Valid values: '0', '1', 'true', 'false'",
                            s
                        )));
                    }
                };
                Ok(Some(ColumnValues::Bit(bit_value)))
            }

            // String → Decimal/Numeric: Parse string as decimal
            (SourcePythonType::String, SqlDbType::Decimal | SqlDbType::Numeric) => {
                let result = Self::coerce_string_to_decimal(py_obj, target_meta)?;
                Ok(Some(result))
            }

            // Int → Decimal/Numeric: Convert integer to decimal
            (SourcePythonType::Int, SqlDbType::Decimal | SqlDbType::Numeric) => {
                let result = Self::coerce_int_to_decimal(py_obj, target_meta)?;
                Ok(Some(result))
            }

            // Float → Decimal/Numeric: Convert float to decimal
            (SourcePythonType::Float, SqlDbType::Decimal | SqlDbType::Numeric) => {
                let result = Self::coerce_float_to_decimal(py_obj, target_meta)?;
                Ok(Some(result))
            }

            // Decimal → Decimal/Numeric: Validate precision/scale
            (SourcePythonType::Decimal, SqlDbType::Decimal | SqlDbType::Numeric) => {
                let result = Self::coerce_decimal_to_decimal(py_obj, target_meta)?;
                Ok(Some(result))
            }

            // String → Money/SmallMoney: Parse string as money
            (SourcePythonType::String, SqlDbType::Money | SqlDbType::SmallMoney) => {
                let result = Self::coerce_string_to_money(py_obj, target_meta.sql_type)?;
                Ok(Some(result))
            }

            // Int → Money/SmallMoney: Convert integer to money
            (SourcePythonType::Int, SqlDbType::Money | SqlDbType::SmallMoney) => {
                let result = Self::coerce_int_to_money(py_obj, target_meta.sql_type)?;
                Ok(Some(result))
            }

            // Float → Money/SmallMoney: Convert float to money
            (SourcePythonType::Float, SqlDbType::Money | SqlDbType::SmallMoney) => {
                let result = Self::coerce_float_to_money(py_obj, target_meta.sql_type)?;
                Ok(Some(result))
            }

            // Decimal → Money/SmallMoney: Convert decimal to money
            (SourcePythonType::Decimal, SqlDbType::Money | SqlDbType::SmallMoney) => {
                let result = Self::coerce_decimal_to_money(py_obj, target_meta.sql_type)?;
                Ok(Some(result))
            }

            // DateTime → Date: Extract date part
            (SourcePythonType::DateTime, SqlDbType::Date) => {
                let result = Self::coerce_datetime_to_date(py_obj)?;
                Ok(Some(result))
            }

            // String → Date: Parse ISO format date string (YYYY-MM-DD)
            (SourcePythonType::String, SqlDbType::Date) => {
                let result = Self::coerce_string_to_date(py_obj)?;
                Ok(Some(result))
            }

            // String → Time: Parse ISO format time string (HH:MM:SS or HH:MM:SS.ffffff)
            (SourcePythonType::String, SqlDbType::Time) => {
                let result = Self::coerce_string_to_time(py_obj)?;
                Ok(Some(result))
            }

            // String → DateTime/SmallDateTime: Parse ISO format datetime string
            (SourcePythonType::String, SqlDbType::DateTime | SqlDbType::SmallDateTime) => {
                let result = Self::coerce_string_to_datetime(py_obj, target_meta.sql_type)?;
                Ok(Some(result))
            }

            (SourcePythonType::String, SqlDbType::DateTime2 | SqlDbType::DateTimeOffset) => {
                let result = Self::coerce_string_to_temporal(py_obj, target_meta)?;
                Ok(Some(result))
            }

            (SourcePythonType::DateTime, SqlDbType::DateTime2 | SqlDbType::DateTimeOffset) => {
                let result = py_to_column_value(py_obj, Some(target_meta))?;
                Ok(Some(result))
            }

            // Float → Float: Direct mapping (no coercion needed, but validate range)
            (SourcePythonType::Float, SqlDbType::Float) => {
                let result = Self::coerce_float_to_float(py_obj)?;
                Ok(Some(result))
            }

            // Float → Real: Convert f64 to f32 with precision loss warning
            (SourcePythonType::Float, SqlDbType::Real) => {
                let result = Self::coerce_float_to_real(py_obj)?;
                Ok(Some(result))
            }

            // String → Float: Parse string as f64
            (SourcePythonType::String, SqlDbType::Float) => {
                let result = Self::coerce_string_to_float(py_obj)?;
                Ok(Some(result))
            }

            // String → Real: Parse string as f32
            (SourcePythonType::String, SqlDbType::Real) => {
                let result = Self::coerce_string_to_real(py_obj)?;
                Ok(Some(result))
            }

            // Int → Float: Convert integer to f64
            (SourcePythonType::Int, SqlDbType::Float) => {
                let result = Self::coerce_int_to_float(py_obj)?;
                Ok(Some(result))
            }

            // Int → Real: Convert integer to f32
            (SourcePythonType::Int, SqlDbType::Real) => {
                let result = Self::coerce_int_to_real(py_obj)?;
                Ok(Some(result))
            }

            // String → JSON: Parse string as JSON (validate it's valid JSON)
            (SourcePythonType::String, SqlDbType::Json) => {
                let result = Self::coerce_string_to_json(py_obj)?;
                Ok(Some(result))
            }

            // Dict → JSON: Serialize Python dict to JSON
            (SourcePythonType::Dict, SqlDbType::Json) => {
                let result = Self::coerce_dict_to_json(py_obj)?;
                Ok(Some(result))
            }

            // List → JSON: Serialize Python list to JSON
            (SourcePythonType::List, SqlDbType::Json) => {
                let result = Self::coerce_list_to_json(py_obj)?;
                Ok(Some(result))
            }

            // String → XML: Convert string to XML
            (SourcePythonType::String, SqlDbType::Xml) => {
                let result = Self::coerce_string_to_xml(py_obj)?;
                Ok(Some(result))
            }

            // Int → NVarChar/VarChar/NChar/Char/NText/Text: Convert integer to string
            (
                SourcePythonType::Int,
                SqlDbType::NVarChar
                | SqlDbType::VarChar
                | SqlDbType::NChar
                | SqlDbType::Char
                | SqlDbType::NText
                | SqlDbType::Text,
            ) => {
                let result = Self::coerce_int_to_string(py_obj)?;
                Ok(Some(result))
            }

            // Float → NVarChar/VarChar/NChar/Char/NText/Text: Convert float to string
            (
                SourcePythonType::Float,
                SqlDbType::NVarChar
                | SqlDbType::VarChar
                | SqlDbType::NChar
                | SqlDbType::Char
                | SqlDbType::NText
                | SqlDbType::Text,
            ) => {
                let result = Self::coerce_float_to_string(py_obj)?;
                Ok(Some(result))
            }

            // Bool → NVarChar/VarChar/NChar/Char/NText/Text: Convert boolean to string ('True'/'False')
            (
                SourcePythonType::Bool,
                SqlDbType::NVarChar
                | SqlDbType::VarChar
                | SqlDbType::NChar
                | SqlDbType::Char
                | SqlDbType::NText
                | SqlDbType::Text,
            ) => {
                let result = Self::coerce_bool_to_string(py_obj)?;
                Ok(Some(result))
            }

            // Decimal → NVarChar/VarChar/NChar/Char/NText/Text: Convert decimal to string
            (
                SourcePythonType::Decimal,
                SqlDbType::NVarChar
                | SqlDbType::VarChar
                | SqlDbType::NChar
                | SqlDbType::Char
                | SqlDbType::NText
                | SqlDbType::Text,
            ) => {
                let result = Self::coerce_decimal_to_string(py_obj)?;
                Ok(Some(result))
            }

            // List → Vector: Convert Python list to SQL Server vector type
            (SourcePythonType::List, SqlDbType::Vector) => {
                let result = Self::coerce_list_to_vector(py_obj, target_meta)?;
                Ok(Some(result))
            }

            // String → Vector: Parse JSON float array string to VECTOR
            (SourcePythonType::String, SqlDbType::Vector) => {
                let result = PythonRowAdapter::coerce_string_to_vector(py_obj, target_meta)?;
                Ok(Some(result))
            }

            // String → UniqueIdentifier: Parse UUID string to UNIQUEIDENTIFIER
            (SourcePythonType::String, SqlDbType::UniqueIdentifier) => {
                let result = Self::coerce_string_to_uuid(py_obj)?;
                Ok(Some(result))
            }

            // No coercion needed - use default conversion
            _ => Ok(None),
        }
    }

    /// Coerce a Python string to a SQL Server integer type.
    ///
    /// Parses the string as i64 and validates it fits within the target integer type's range.
    fn coerce_string_to_integer(
        py_obj: &Bound<'_, PyAny>,
        target_type: SqlDbType,
    ) -> TdsResult<ColumnValues> {
        let py_str = py_obj
            .cast::<PyString>()
            .map_err(|e| Error::UsageError(format!("Failed to cast to string: {}", e)))?;

        let s = py_str
            .to_str()
            .map_err(|e| Error::UsageError(format!("Failed to extract string: {}", e)))?;

        // Parse as i64 to cover all integer types
        let parsed = s.parse::<i64>().map_err(|e| {
            Error::UsageError(format!("Cannot convert string '{}' to integer: {}", s, e))
        })?;

        // Convert to appropriate TDS integer type with range validation
        match target_type {
            SqlDbType::TinyInt => {
                if (0..=255).contains(&parsed) {
                    Ok(ColumnValues::TinyInt(parsed as u8))
                } else {
                    Err(Error::UsageError(format!(
                        "Value {} out of range for TINYINT (0-255)",
                        parsed
                    )))
                }
            }
            SqlDbType::SmallInt => {
                if parsed >= i16::MIN as i64 && parsed <= i16::MAX as i64 {
                    Ok(ColumnValues::SmallInt(parsed as i16))
                } else {
                    Err(Error::UsageError(format!(
                        "Value {} out of range for SMALLINT ({} to {})",
                        parsed,
                        i16::MIN,
                        i16::MAX
                    )))
                }
            }
            SqlDbType::Int => {
                if parsed >= i32::MIN as i64 && parsed <= i32::MAX as i64 {
                    Ok(ColumnValues::Int(parsed as i32))
                } else {
                    Err(Error::UsageError(format!(
                        "Value {} out of range for INT ({} to {})",
                        parsed,
                        i32::MIN,
                        i32::MAX
                    )))
                }
            }
            SqlDbType::BigInt => Ok(ColumnValues::BigInt(parsed)),
            _ => unreachable!("coerce_string_to_integer called with non-integer target type"),
        }
    }

    /// Coerce a Python integer to a SQL Server integer type with range validation.
    ///
    /// Extracts the Python int and validates it fits within the target integer type's range.
    /// This prevents Python integers that exceed the target type's range from being silently
    /// converted to a different SQL integer type.
    ///
    /// For BIGINT targets, this function will attempt to extract the value as i64. If the Python
    /// integer is too large to fit in i64 (exceeds i64::MAX or i64::MIN), PyO3's extract will
    /// fail with an OverflowError, which we catch and convert to a meaningful error message.
    fn coerce_python_int_to_integer(
        py_obj: &Bound<'_, PyAny>,
        target_type: SqlDbType,
    ) -> TdsResult<ColumnValues> {
        // Extract as i64 to cover all integer types
        // For Python integers larger than i64::MAX, this will raise OverflowError
        let value = py_obj.extract::<i64>().map_err(|e| {
            // Check if it's an overflow error (Python int too large for i64)
            if e.to_string().contains("OverflowError") || e.to_string().contains("overflow") {
                Error::UsageError(format!(
                    "Python integer out of range for BIGINT column (valid range: {} to {})",
                    i64::MIN,
                    i64::MAX
                ))
            } else {
                Error::UsageError(format!("Cannot extract Python integer as i64: {}", e))
            }
        })?;

        // Convert to appropriate TDS integer type with range validation
        match target_type {
            SqlDbType::TinyInt => {
                if (0..=255).contains(&value) {
                    Ok(ColumnValues::TinyInt(value as u8))
                } else {
                    Err(Error::UsageError(format!(
                        "Python integer {} out of range for TINYINT column (valid range: 0-255)",
                        value
                    )))
                }
            }
            SqlDbType::SmallInt => {
                if value >= i16::MIN as i64 && value <= i16::MAX as i64 {
                    Ok(ColumnValues::SmallInt(value as i16))
                } else {
                    Err(Error::UsageError(format!(
                        "Python integer {} out of range for SMALLINT column (valid range: {} to {})",
                        value,
                        i16::MIN,
                        i16::MAX
                    )))
                }
            }
            SqlDbType::Int => {
                if value >= i32::MIN as i64 && value <= i32::MAX as i64 {
                    Ok(ColumnValues::Int(value as i32))
                } else {
                    Err(Error::UsageError(format!(
                        "Python integer {} out of range for INT column (valid range: {} to {})",
                        value,
                        i32::MIN,
                        i32::MAX
                    )))
                }
            }
            SqlDbType::BigInt => {
                // Value already validated to fit in i64 by extract() above
                Ok(ColumnValues::BigInt(value))
            }
            _ => unreachable!("coerce_python_int_to_integer called with non-integer target type"),
        }
    }

    /// Coerce a Python string to a SQL Server DECIMAL/NUMERIC type.
    fn coerce_string_to_decimal(
        py_obj: &Bound<'_, PyAny>,
        target_meta: &BulkCopyColumnMetadata,
    ) -> TdsResult<ColumnValues> {
        let s = py_obj
            .extract::<String>()
            .map_err(|e| Error::UsageError(format!("Failed to extract string: {}", e)))?;

        let decimal_parts =
            DecimalParts::from_string(&s, target_meta.precision, target_meta.scale)?;

        if target_meta.sql_type == SqlDbType::Numeric {
            Ok(ColumnValues::Numeric(decimal_parts))
        } else {
            Ok(ColumnValues::Decimal(decimal_parts))
        }
    }

    /// Coerce a Python integer to a SQL Server DECIMAL/NUMERIC type.
    fn coerce_int_to_decimal(
        py_obj: &Bound<'_, PyAny>,
        target_meta: &BulkCopyColumnMetadata,
    ) -> TdsResult<ColumnValues> {
        let value = py_obj
            .extract::<i64>()
            .map_err(|e| Error::UsageError(format!("Failed to extract integer: {}", e)))?;

        let decimal_parts =
            DecimalParts::from_i64(value, target_meta.precision, target_meta.scale)?;

        if target_meta.sql_type == SqlDbType::Numeric {
            Ok(ColumnValues::Numeric(decimal_parts))
        } else {
            Ok(ColumnValues::Decimal(decimal_parts))
        }
    }

    /// Coerce a Python float to a SQL Server DECIMAL/NUMERIC type.
    fn coerce_float_to_decimal(
        py_obj: &Bound<'_, PyAny>,
        target_meta: &BulkCopyColumnMetadata,
    ) -> TdsResult<ColumnValues> {
        let value = py_obj
            .extract::<f64>()
            .map_err(|e| Error::UsageError(format!("Failed to extract float: {}", e)))?;

        let decimal_parts =
            DecimalParts::from_f64(value, target_meta.precision, target_meta.scale)?;

        if target_meta.sql_type == SqlDbType::Numeric {
            Ok(ColumnValues::Numeric(decimal_parts))
        } else {
            Ok(ColumnValues::Decimal(decimal_parts))
        }
    }

    /// Coerce a Python Decimal to a SQL Server DECIMAL/NUMERIC type.
    /// Validates that the Python Decimal's precision and scale match the target.
    fn coerce_decimal_to_decimal(
        py_obj: &Bound<'_, PyAny>,
        target_meta: &BulkCopyColumnMetadata,
    ) -> TdsResult<ColumnValues> {
        // Extract Decimal as string
        if let Ok(decimal_str) = py_obj.call_method0("__str__")
            && let Ok(s) = decimal_str.extract::<String>()
        {
            let decimal_parts =
                DecimalParts::from_string(&s, target_meta.precision, target_meta.scale)?;

            if target_meta.sql_type == SqlDbType::Numeric {
                return Ok(ColumnValues::Numeric(decimal_parts));
            } else {
                return Ok(ColumnValues::Decimal(decimal_parts));
            }
        }

        Err(Error::UsageError(
            "Failed to extract Decimal value as string".to_string(),
        ))
    }

    /// Coerce a Python string to a SQL Server MONEY/SMALLMONEY type.
    fn coerce_string_to_money(
        py_obj: &Bound<'_, PyAny>,
        target_type: SqlDbType,
    ) -> TdsResult<ColumnValues> {
        let py_str = py_obj
            .cast::<PyString>()
            .map_err(|e| Error::UsageError(format!("Failed to cast to string: {}", e)))?;

        let s = py_str
            .to_str()
            .map_err(|e| Error::UsageError(format!("Failed to extract string: {}", e)))?;

        // Parse the string as f64, then convert to money
        let value = s.parse::<f64>().map_err(|e| {
            Error::UsageError(format!(
                "Failed to parse string '{}' as money value: {}",
                s, e
            ))
        })?;

        Self::float_to_money(value, target_type)
    }

    /// Coerce a Python integer to a SQL Server MONEY/SMALLMONEY type.
    fn coerce_int_to_money(
        py_obj: &Bound<'_, PyAny>,
        target_type: SqlDbType,
    ) -> TdsResult<ColumnValues> {
        let value = py_obj
            .extract::<i64>()
            .map_err(|e| Error::UsageError(format!("Failed to extract integer: {}", e)))?;

        // Convert to f64, then to money (money values are scaled by 10,000)
        let money_value = value as f64;
        Self::float_to_money(money_value, target_type)
    }

    /// Coerce a Python float to a SQL Server MONEY/SMALLMONEY type.
    fn coerce_float_to_money(
        py_obj: &Bound<'_, PyAny>,
        target_type: SqlDbType,
    ) -> TdsResult<ColumnValues> {
        let value = py_obj
            .extract::<f64>()
            .map_err(|e| Error::UsageError(format!("Failed to extract float: {}", e)))?;

        Self::float_to_money(value, target_type)
    }

    /// Coerce a Python Decimal to a SQL Server MONEY/SMALLMONEY type.
    /// Uses rust_decimal for precision-preserving conversion without f64 loss.
    fn coerce_decimal_to_money(
        py_obj: &Bound<'_, PyAny>,
        target_type: SqlDbType,
    ) -> TdsResult<ColumnValues> {
        // Extract Decimal as string
        if let Ok(decimal_str) = py_obj.call_method0("__str__")
            && let Ok(s) = decimal_str.extract::<String>()
        {
            // Parse using rust_decimal - handles precision perfectly
            let decimal = Decimal::from_str(&s).map_err(|e| {
                Error::UsageError(format!("Failed to parse Decimal '{}': {}", s, e))
            })?;

            return Self::decimal_to_money(decimal, target_type);
        }

        Err(Error::UsageError(
            "Failed to extract Decimal value for money conversion".to_string(),
        ))
    }

    /// Convert rust_decimal::Decimal to MONEY/SMALLMONEY
    /// Money values are scaled by 10,000 and stored as integers.
    /// This approach avoids f64 precision loss for large or high-precision values.
    fn decimal_to_money(decimal: Decimal, target_type: SqlDbType) -> TdsResult<ColumnValues> {
        // Money types have exactly 4 decimal places
        const MONEY_SCALE: u32 = 4;

        // Scale to 4 decimal places (banker's rounding is default in rust_decimal)
        let scaled_decimal = decimal.round_dp(MONEY_SCALE);

        // Multiply by 10,000 to get the integer representation
        let money_decimal = scaled_decimal * Decimal::from(10000);

        // Convert to i64 (this will return None if out of range)
        let scaled_value = money_decimal.to_i64().ok_or_else(|| {
            Error::UsageError(format!(
                "Decimal value {} out of range for money conversion",
                decimal
            ))
        })?;

        match target_type {
            SqlDbType::SmallMoney => {
                // SMALLMONEY range: -214,748.3648 to 214,748.3647
                // Stored as i32, so range is -2,147,483,648 to 2,147,483,647 (scaled by 10000)
                const SMALLMONEY_MIN: i64 = -2_147_483_648;
                const SMALLMONEY_MAX: i64 = 2_147_483_647;

                if !(SMALLMONEY_MIN..=SMALLMONEY_MAX).contains(&scaled_value) {
                    return Err(Error::UsageError(format!(
                        "Value {} exceeds SMALLMONEY range (-214748.3648 to 214748.3647)",
                        decimal
                    )));
                }

                Ok(ColumnValues::SmallMoney(
                    mssql_tds::datatypes::column_values::SqlSmallMoney {
                        int_val: scaled_value as i32,
                    },
                ))
            }
            SqlDbType::Money => {
                // MONEY range: -922,337,203,685,477.5808 to 922,337,203,685,477.5807
                // Fits in i64 when scaled by 10000
                let lsb_part = (scaled_value & 0xFFFFFFFF) as i32;
                let msb_part = (scaled_value >> 32) as i32;

                Ok(ColumnValues::Money(
                    mssql_tds::datatypes::column_values::SqlMoney { lsb_part, msb_part },
                ))
            }
            _ => Err(Error::UsageError(format!(
                "Invalid target type {:?} for money conversion",
                target_type
            ))),
        }
    }

    /// Helper: Convert f64 to MONEY or SMALLMONEY with range validation.
    ///
    /// Money values are stored as scaled integers (scaled by 10,000):
    /// - MONEY: 8-byte integer (two 4-byte parts: MSB, LSB)
    /// - SMALLMONEY: 4-byte integer
    fn float_to_money(value: f64, target_type: SqlDbType) -> TdsResult<ColumnValues> {
        // Money types have 4 decimal places and are stored as integers scaled by 10,000
        const MONEY_SCALE: f64 = 10000.0;

        // MONEY range: -922,337,203,685,477.5808 to 922,337,203,685,477.5807
        const MONEY_MIN: f64 = -922_337_203_685_477.6;
        const MONEY_MAX: f64 = 922_337_203_685_477.6;

        // SMALLMONEY range: -214,748.3648 to 214,748.3647
        const SMALLMONEY_MIN: f64 = -214_748.364_8;
        const SMALLMONEY_MAX: f64 = 214_748.364_7;

        match target_type {
            SqlDbType::SmallMoney => {
                // Validate range for SMALLMONEY
                if !(SMALLMONEY_MIN..=SMALLMONEY_MAX).contains(&value) {
                    return Err(Error::UsageError(format!(
                        "Value {} exceeds SMALLMONEY range ({} to {})",
                        value, SMALLMONEY_MIN, SMALLMONEY_MAX
                    )));
                }

                // Scale by 10,000 and convert to i32
                let scaled = (value * MONEY_SCALE).round() as i32;

                Ok(ColumnValues::SmallMoney(
                    mssql_tds::datatypes::column_values::SqlSmallMoney { int_val: scaled },
                ))
            }
            SqlDbType::Money => {
                // Validate range for MONEY
                if !(MONEY_MIN..=MONEY_MAX).contains(&value) {
                    return Err(Error::UsageError(format!(
                        "Value {} exceeds MONEY range ({} to {})",
                        value, MONEY_MIN, MONEY_MAX
                    )));
                }

                // Scale by 10,000 and convert to i64
                let scaled = (value * MONEY_SCALE).round() as i64;

                // Split into MSB (high 32 bits) and LSB (low 32 bits)
                let lsb_part = (scaled & 0xFFFFFFFF) as i32;
                let msb_part = (scaled >> 32) as i32;

                Ok(ColumnValues::Money(
                    mssql_tds::datatypes::column_values::SqlMoney { lsb_part, msb_part },
                ))
            }
            _ => Err(Error::UsageError(format!(
                "Invalid target type {:?} for money conversion",
                target_type
            ))),
        }
    }

    /// Coerce a Python string to SQL Server DATE type.
    ///
    /// Parses ISO format date strings (YYYY-MM-DD) into SqlDate values.
    fn coerce_string_to_date(py_obj: &Bound<'_, PyAny>) -> TdsResult<ColumnValues> {
        let py_str = py_obj
            .cast::<PyString>()
            .map_err(|e| Error::UsageError(format!("Failed to cast to string: {}", e)))?;

        let s = py_str
            .to_str()
            .map_err(|e| Error::UsageError(format!("Failed to extract string: {}", e)))?;

        // Use Python's datetime.date.fromisoformat() to parse ISO date string
        // This handles all ISO format variations and date validation automatically
        let py = py_obj.py();
        let datetime_module = py
            .import("datetime")
            .map_err(|e| Error::UsageError(format!("Failed to import datetime module: {}", e)))?;

        let date_class = datetime_module
            .getattr("date")
            .map_err(|e| Error::UsageError(format!("Failed to get date class: {}", e)))?;

        let parsed_date = date_class
            .call_method1("fromisoformat", (s,))
            .map_err(|e| {
                Error::UsageError(format!(
                    "Invalid ISO date format '{}'. Expected YYYY-MM-DD: {}",
                    s, e
                ))
            })?;

        // Use Python's toordinal() to get ordinal (1-based: date(1,1,1) = 1)
        // SQL Server DATE needs 0-based days since 0001-01-01, so subtract 1
        let days_py = parsed_date
            .call_method0("toordinal")
            .map_err(|e| Error::UsageError(format!("Failed to get ordinal from date: {}", e)))?;

        let ordinal = days_py
            .extract::<u32>()
            .map_err(|e| Error::UsageError(format!("Failed to convert ordinal to u32: {}", e)))?;

        // Convert from 1-based ordinal to 0-based days since 0001-01-01
        let days = ordinal
            .checked_sub(1)
            .ok_or_else(|| Error::UsageError("Date ordinal is 0, expected >= 1".to_string()))?;

        Ok(ColumnValues::Date(
            mssql_tds::datatypes::column_values::SqlDate::create(days)?,
        ))
    }

    /// Coerce a Python datetime to SQL Server DATE type (extract date part).
    ///
    /// Extracts the date part from a datetime object, discarding the time component.
    fn coerce_datetime_to_date(py_obj: &Bound<'_, PyAny>) -> TdsResult<ColumnValues> {
        // Convert datetime to date by calling .date() method
        let date_obj = py_obj.call_method0("date").map_err(|e| {
            Error::UsageError(format!("Failed to extract date from datetime: {}", e))
        })?;

        // Use Python's toordinal() to get ordinal (1-based: date(1,1,1) = 1)
        // SQL Server DATE needs 0-based days since 0001-01-01, so subtract 1
        let days_py = date_obj
            .call_method0("toordinal")
            .map_err(|e| Error::UsageError(format!("Failed to get ordinal from date: {}", e)))?;

        let ordinal = days_py
            .extract::<u32>()
            .map_err(|e| Error::UsageError(format!("Failed to convert ordinal to u32: {}", e)))?;

        // Convert from 1-based ordinal to 0-based days since 0001-01-01
        let days = ordinal
            .checked_sub(1)
            .ok_or_else(|| Error::UsageError("Date ordinal is 0, expected >= 1".to_string()))?;

        Ok(ColumnValues::Date(
            mssql_tds::datatypes::column_values::SqlDate::create(days)?,
        ))
    }

    /// Coerce a Python string to SQL Server TIME type.
    ///
    /// Parses ISO format time strings (HH:MM:SS or HH:MM:SS.ffffff) to TIME.
    fn coerce_string_to_time(py_obj: &Bound<'_, PyAny>) -> TdsResult<ColumnValues> {
        let py_str = py_obj
            .cast::<PyString>()
            .map_err(|e| Error::UsageError(format!("Failed to cast to string: {}", e)))?;

        let s = py_str
            .to_str()
            .map_err(|e| Error::UsageError(format!("Failed to extract string: {}", e)))?;

        // Use Python's datetime.time.fromisoformat() to parse ISO time string
        // This handles all ISO format variations and time validation automatically
        let py = py_obj.py();
        let datetime_module = py
            .import("datetime")
            .map_err(|e| Error::UsageError(format!("Failed to import datetime module: {}", e)))?;

        let time_class = datetime_module
            .getattr("time")
            .map_err(|e| Error::UsageError(format!("Failed to get time class: {}", e)))?;

        let parsed_time = time_class
            .call_method1("fromisoformat", (s,))
            .map_err(|e| {
                Error::UsageError(format!(
                    "Invalid ISO time format '{}'. Expected HH:MM:SS or HH:MM:SS.ffffff: {}",
                    s, e
                ))
            })?;

        // Extract hour, minute, second, microsecond
        let hour = parsed_time
            .getattr("hour")
            .and_then(|v| v.extract::<u8>())
            .map_err(|e| Error::UsageError(format!("Failed to get hour from time: {}", e)))?;

        let minute = parsed_time
            .getattr("minute")
            .and_then(|v| v.extract::<u8>())
            .map_err(|e| Error::UsageError(format!("Failed to get minute from time: {}", e)))?;

        let second = parsed_time
            .getattr("second")
            .and_then(|v| v.extract::<u8>())
            .map_err(|e| Error::UsageError(format!("Failed to get second from time: {}", e)))?;

        let microsecond = parsed_time
            .getattr("microsecond")
            .and_then(|v| v.extract::<u32>())
            .map_err(|e| {
                Error::UsageError(format!("Failed to get microsecond from time: {}", e))
            })?;

        // Convert to 100-nanosecond units (SQL Server TIME uses 100ns precision)
        // time_100ns = hour * 3600 * 10^7 + minute * 60 * 10^7 + second * 10^7 + microsecond * 10
        let time_nanoseconds = (hour as u64) * 36_000_000_000
            + (minute as u64) * 600_000_000
            + (second as u64) * 10_000_000
            + (microsecond as u64) * 10;

        Ok(ColumnValues::Time(
            mssql_tds::datatypes::column_values::SqlTime {
                time_nanoseconds,
                scale: 7, // Use maximum scale (100ns precision)
            },
        ))
    }

    /// Coerce a Python string to SQL Server DATETIME or SMALLDATETIME.
    ///
    /// Parses ISO format datetime strings (YYYY-MM-DD HH:MM:SS or YYYY-MM-DDTHH:MM:SS)
    /// and converts them to SqlDateTime or SqlSmallDateTime format depending on target type.
    fn coerce_string_to_datetime(
        py_obj: &Bound<'_, PyAny>,
        target_type: SqlDbType,
    ) -> TdsResult<ColumnValues> {
        let py_str = py_obj
            .cast::<PyString>()
            .map_err(|e| Error::UsageError(format!("Failed to cast to string: {}", e)))?;

        let s = py_str
            .to_str()
            .map_err(|e| Error::UsageError(format!("Failed to extract string: {}", e)))?;

        // Use Python's datetime.datetime.fromisoformat() to parse ISO datetime string
        // This handles various ISO format variations automatically
        let py = py_obj.py();
        let datetime_module = py
            .import("datetime")
            .map_err(|e| Error::UsageError(format!("Failed to import datetime module: {}", e)))?;

        let datetime_class = datetime_module
            .getattr("datetime")
            .map_err(|e| Error::UsageError(format!("Failed to get datetime class: {}", e)))?;

        // Replace space with 'T' for ISO format compatibility if needed
        let iso_str = s.replace(' ', "T");

        let parsed_datetime = datetime_class
            .call_method1("fromisoformat", (iso_str.as_str(),))
            .map_err(|e| {
                Error::UsageError(format!(
                    "Invalid datetime format '{}'. Expected ISO format like '2024-01-15 09:30:00' or '2024-01-15T09:30:00': {}",
                    s, e
                ))
            })?;

        // Extract components from parsed datetime
        let year = parsed_datetime
            .getattr("year")
            .and_then(|v| v.extract::<i32>())
            .map_err(|e| Error::UsageError(format!("Failed to get year from datetime: {}", e)))?;

        let month = parsed_datetime
            .getattr("month")
            .and_then(|v| v.extract::<u8>())
            .map_err(|e| Error::UsageError(format!("Failed to get month from datetime: {}", e)))?;

        let day = parsed_datetime
            .getattr("day")
            .and_then(|v| v.extract::<u8>())
            .map_err(|e| Error::UsageError(format!("Failed to get day from datetime: {}", e)))?;

        let hour = parsed_datetime
            .getattr("hour")
            .and_then(|v| v.extract::<u8>())
            .map_err(|e| Error::UsageError(format!("Failed to get hour from datetime: {}", e)))?;

        let minute = parsed_datetime
            .getattr("minute")
            .and_then(|v| v.extract::<u8>())
            .map_err(|e| Error::UsageError(format!("Failed to get minute from datetime: {}", e)))?;

        let second = parsed_datetime
            .getattr("second")
            .and_then(|v| v.extract::<u8>())
            .map_err(|e| Error::UsageError(format!("Failed to get second from datetime: {}", e)))?;

        let microsecond = parsed_datetime
            .getattr("microsecond")
            .and_then(|v| v.extract::<u32>())
            .map_err(|e| {
                Error::UsageError(format!("Failed to get microsecond from datetime: {}", e))
            })?;

        // Calculate days since 1900-01-01
        let date_class = datetime_module
            .getattr("date")
            .map_err(|e| Error::UsageError(format!("Failed to get date class: {}", e)))?;

        let base_date = date_class
            .call1((1900, 1, 1))
            .map_err(|e| Error::UsageError(format!("Failed to create base date: {}", e)))?;

        let base_ordinal = base_date
            .call_method0("toordinal")
            .and_then(|v| v.extract::<i32>())
            .map_err(|e| Error::UsageError(format!("Failed to get base ordinal: {}", e)))?;

        let current_date = date_class
            .call1((year, month, day))
            .map_err(|e| Error::UsageError(format!("Failed to create current date: {}", e)))?;

        let current_ordinal = current_date
            .call_method0("toordinal")
            .and_then(|v| v.extract::<i32>())
            .map_err(|e| Error::UsageError(format!("Failed to get current ordinal: {}", e)))?;

        let days = current_ordinal - base_ordinal;

        // Check if target is SmallDateTime
        if target_type == SqlDbType::SmallDateTime {
            // Validate SMALLDATETIME range: 1900-01-01 00:00:00 to 2079-06-06 23:59:59
            if !(0..=65535).contains(&days) {
                return Err(Error::UsageError(format!(
                    "DateTime value {}-{:02}-{:02} out of range for SMALLDATETIME (valid range: 1900-01-01 to 2079-06-06)",
                    year, month, day
                )));
            }

            // Calculate time in minutes since midnight with proper rounding
            // SMALLDATETIME uses minute precision - round seconds >= 30 up to next minute
            // This matches SQL Server's client-side behavior: add 30 seconds before converting
            let mut rounded_minute = minute;
            let mut rounded_hour = hour;
            let mut rounded_days = days;

            if second >= 30 {
                rounded_minute += 1;
                if rounded_minute >= 60 {
                    rounded_minute = 0;
                    rounded_hour += 1;
                    if rounded_hour >= 24 {
                        rounded_hour = 0;
                        rounded_days += 1;
                    }
                }
            }

            // Validate again after rounding (could overflow into next day beyond max date)
            if !(0..=65535).contains(&rounded_days) {
                return Err(Error::UsageError(format!(
                    "DateTime value {}-{:02}-{:02} {hour:02}:{minute:02}:{second:02} out of range for SMALLDATETIME after rounding (valid range: 1900-01-01 to 2079-06-06)",
                    year, month, day
                )));
            }

            let time_minutes = (rounded_hour as u16) * 60 + (rounded_minute as u16);

            return Ok(ColumnValues::SmallDateTime(
                mssql_tds::datatypes::column_values::SqlSmallDateTime {
                    days: days as u16,
                    time: time_minutes,
                },
            ));
        }

        // Default to DATETIME format
        let (final_days, time_ticks) =
            crate::types::datetime_to_ticks(days, hour, minute, second, microsecond)?;

        Ok(ColumnValues::DateTime(
            mssql_tds::datatypes::column_values::SqlDateTime {
                days: final_days,
                time: time_ticks,
            },
        ))
    }

    fn coerce_string_to_temporal(
        py_obj: &Bound<'_, PyAny>,
        target_meta: &BulkCopyColumnMetadata,
    ) -> TdsResult<ColumnValues> {
        let py_str = py_obj
            .cast::<PyString>()
            .map_err(|e| Error::UsageError(format!("Failed to cast to string: {}", e)))?;

        let s = py_str
            .to_str()
            .map_err(|e| Error::UsageError(format!("Failed to extract string: {}", e)))?;

        parse_bulk_datetime(s, target_meta)
    }

    /// Coerce a Python float to a SQL Server FLOAT (f64).
    ///
    /// This is a direct mapping with no precision loss.
    fn coerce_float_to_float(py_obj: &Bound<'_, PyAny>) -> TdsResult<ColumnValues> {
        let value = py_obj
            .extract::<f64>()
            .map_err(|e| Error::UsageError(format!("Cannot extract Python float: {}", e)))?;

        // Validate for special values
        if value.is_nan() || value.is_infinite() {
            return Err(Error::UsageError(format!(
                "Cannot convert special float value {} to SQL FLOAT",
                value
            )));
        }

        Ok(ColumnValues::Float(value))
    }

    /// Coerce a Python float to a SQL Server REAL (f32).
    ///
    /// This involves converting f64 to f32, which may result in precision loss.
    fn coerce_float_to_real(py_obj: &Bound<'_, PyAny>) -> TdsResult<ColumnValues> {
        let value = py_obj
            .extract::<f64>()
            .map_err(|e| Error::UsageError(format!("Cannot extract Python float: {}", e)))?;

        // Validate for special values
        if value.is_nan() || value.is_infinite() {
            return Err(Error::UsageError(format!(
                "Cannot convert special float value {} to SQL REAL",
                value
            )));
        }

        // Convert f64 to f32 - may lose precision
        let real_value = value as f32;

        // Check if conversion resulted in infinity (overflow)
        if real_value.is_infinite() {
            return Err(Error::UsageError(format!(
                "Value {} is out of range for SQL REAL (±3.40E+38)",
                value
            )));
        }

        Ok(ColumnValues::Real(real_value))
    }

    /// Coerce a Python string to a SQL Server FLOAT (f64).
    ///
    /// Parses the string as a floating-point number.
    fn coerce_string_to_float(py_obj: &Bound<'_, PyAny>) -> TdsResult<ColumnValues> {
        let py_str = py_obj
            .cast::<PyString>()
            .map_err(|e| Error::UsageError(format!("Failed to cast to string: {}", e)))?;

        let s = py_str
            .to_str()
            .map_err(|e| Error::UsageError(format!("Failed to extract string: {}", e)))?;

        let parsed = s.parse::<f64>().map_err(|e| {
            Error::UsageError(format!("Cannot convert string '{}' to FLOAT: {}", s, e))
        })?;

        // Validate for special values
        if parsed.is_nan() || parsed.is_infinite() {
            return Err(Error::UsageError(format!(
                "Cannot convert special float value {} to SQL FLOAT",
                parsed
            )));
        }

        Ok(ColumnValues::Float(parsed))
    }

    /// Coerce a Python string to a SQL Server REAL (f32).
    ///
    /// Parses the string as a floating-point number and converts to f32.
    fn coerce_string_to_real(py_obj: &Bound<'_, PyAny>) -> TdsResult<ColumnValues> {
        let py_str = py_obj
            .cast::<PyString>()
            .map_err(|e| Error::UsageError(format!("Failed to cast to string: {}", e)))?;

        let s = py_str
            .to_str()
            .map_err(|e| Error::UsageError(format!("Failed to extract string: {}", e)))?;

        let parsed = s.parse::<f32>().map_err(|e| {
            Error::UsageError(format!("Cannot convert string '{}' to REAL: {}", s, e))
        })?;

        // Validate for special values
        if parsed.is_nan() || parsed.is_infinite() {
            return Err(Error::UsageError(format!(
                "Cannot convert special float value {} to SQL REAL",
                parsed
            )));
        }

        Ok(ColumnValues::Real(parsed))
    }

    /// Coerce a Python integer to a SQL Server FLOAT (f64).
    ///
    /// Converts the integer to a float with potential precision loss for very large integers.
    fn coerce_int_to_float(py_obj: &Bound<'_, PyAny>) -> TdsResult<ColumnValues> {
        let value = py_obj
            .extract::<i64>()
            .map_err(|e| Error::UsageError(format!("Cannot extract Python integer: {}", e)))?;

        // Convert to f64
        let float_value = value as f64;

        Ok(ColumnValues::Float(float_value))
    }

    /// Coerce a Python integer to a SQL Server REAL (f32).
    ///
    /// Converts the integer to a float with potential precision loss.
    fn coerce_int_to_real(py_obj: &Bound<'_, PyAny>) -> TdsResult<ColumnValues> {
        let value = py_obj
            .extract::<i64>()
            .map_err(|e| Error::UsageError(format!("Cannot extract Python integer: {}", e)))?;

        // Convert to f32
        let real_value = value as f32;

        // Check if conversion resulted in infinity (overflow)
        if real_value.is_infinite() {
            return Err(Error::UsageError(format!(
                "Value {} is out of range for SQL REAL (±3.40E+38)",
                value
            )));
        }

        Ok(ColumnValues::Real(real_value))
    }

    /// Validates that the source tuple has the same number of columns as the first row.
    fn validate_column_count(tuple_len: usize, expected: Option<usize>) -> TdsResult<()> {
        if let Some(expected) = expected
            && tuple_len != expected
        {
            return Err(Error::UsageError(format!(
                "Row has {} columns, but expected {} columns based on the first row. \
                 All rows must have the same number of columns.",
                tuple_len, expected
            )));
        }
        Ok(())
    }

    /// Target-type-driven value extraction — skips source type detection entirely.
    ///
    /// For the common bulk copy case where we know the destination SQL type from metadata,
    /// we can extract the Python value directly without first detecting the Python type.
    /// Falls back to `convert_with_coercion` for types not on the fast path.
    #[inline]
    fn convert_by_target_type(
        py_obj: &Bound<'_, PyAny>,
        meta: &BulkCopyColumnMetadata,
    ) -> TdsResult<ColumnValues> {
        if py_obj.is_none() {
            return Self::handle_null_value(Some(meta));
        }

        match meta.sql_type {
            SqlDbType::BigInt => match py_obj.extract::<i64>() {
                Ok(val) => Ok(ColumnValues::BigInt(val)),
                Err(_) => Self::convert_with_coercion(py_obj, Some(meta)),
            },
            SqlDbType::Int => match py_obj.extract::<i32>() {
                Ok(val) => Ok(ColumnValues::Int(val)),
                Err(_) => Self::convert_with_coercion(py_obj, Some(meta)),
            },
            SqlDbType::SmallInt => match py_obj.extract::<i16>() {
                Ok(val) => Ok(ColumnValues::SmallInt(val)),
                Err(_) => Self::convert_with_coercion(py_obj, Some(meta)),
            },
            SqlDbType::TinyInt => match py_obj.extract::<u8>() {
                Ok(val) => Ok(ColumnValues::TinyInt(val)),
                Err(_) => Self::convert_with_coercion(py_obj, Some(meta)),
            },
            SqlDbType::Float => match py_obj.extract::<f64>() {
                Ok(val) => Ok(ColumnValues::Float(val)),
                Err(_) => Self::convert_with_coercion(py_obj, Some(meta)),
            },
            SqlDbType::Real => match py_obj.extract::<f32>() {
                Ok(val) => Ok(ColumnValues::Real(val)),
                Err(_) => Self::convert_with_coercion(py_obj, Some(meta)),
            },
            SqlDbType::Bit => match py_obj.extract::<bool>() {
                Ok(val) => Ok(ColumnValues::Bit(val)),
                Err(_) => Self::convert_with_coercion(py_obj, Some(meta)),
            },
            SqlDbType::NVarChar
            | SqlDbType::VarChar
            | SqlDbType::NChar
            | SqlDbType::Char
            | SqlDbType::NText
            | SqlDbType::Text => match py_obj.extract::<String>() {
                Ok(s) => Ok(ColumnValues::String(
                    mssql_tds::datatypes::sql_string::SqlString::from_utf8_string(s),
                )),
                Err(_) => Self::convert_with_coercion(py_obj, Some(meta)),
            },
            _ => Self::convert_with_coercion(py_obj, Some(meta)),
        }
    }

    /// Coerce a Python string to SQL Server JSON.
    ///
    /// Validates that the string contains valid JSON and converts to SqlJson.
    fn coerce_string_to_json(py_obj: &Bound<'_, PyAny>) -> TdsResult<ColumnValues> {
        let py_str = py_obj
            .cast::<PyString>()
            .map_err(|e| Error::UsageError(format!("Failed to cast to string: {}", e)))?;

        let json_str = py_str
            .to_str()
            .map_err(|e| Error::UsageError(format!("Failed to extract string: {}", e)))?;

        // Validate JSON using serde_json
        serde_json::from_str::<serde_json::Value>(json_str)
            .map_err(|e| Error::UsageError(format!("Invalid JSON string: {}", e)))?;

        // Convert to UTF-8 bytes for SqlJson
        let bytes = json_str.as_bytes().to_vec();
        Ok(ColumnValues::Json(SqlJson { bytes }))
    }

    /// Coerce a Python dict to SQL Server JSON.
    ///
    /// Serializes the Python dict to a JSON string using Python's json module.
    fn coerce_dict_to_json(py_obj: &Bound<'_, PyAny>) -> TdsResult<ColumnValues> {
        let py = py_obj.py();
        let json_module = py
            .import("json")
            .map_err(|e| Error::UsageError(format!("Failed to import json module: {}", e)))?;

        // Use json.dumps() to serialize the dict
        let json_str = json_module
            .getattr("dumps")
            .and_then(|dumps| dumps.call1((py_obj,)))
            .and_then(|result| result.extract::<String>())
            .map_err(|e| Error::UsageError(format!("Failed to serialize dict to JSON: {}", e)))?;

        // Convert to UTF-8 bytes for SqlJson
        let bytes = json_str.as_bytes().to_vec();
        Ok(ColumnValues::Json(SqlJson { bytes }))
    }

    /// Coerce a Python list to SQL Server JSON.
    ///
    /// Serializes the Python list to a JSON array string using Python's json module.
    fn coerce_list_to_json(py_obj: &Bound<'_, PyAny>) -> TdsResult<ColumnValues> {
        let py = py_obj.py();
        let json_module = py
            .import("json")
            .map_err(|e| Error::UsageError(format!("Failed to import json module: {}", e)))?;

        // Use json.dumps() to serialize the list
        let json_str = json_module
            .getattr("dumps")
            .and_then(|dumps| dumps.call1((py_obj,)))
            .and_then(|result| result.extract::<String>())
            .map_err(|e| Error::UsageError(format!("Failed to serialize list to JSON: {}", e)))?;

        // Convert to UTF-8 bytes for SqlJson
        let bytes = json_str.as_bytes().to_vec();
        Ok(ColumnValues::Json(SqlJson { bytes }))
    }

    /// Coerce a Python string to SQL Server XML.
    ///
    /// Converts the Python string to SqlXml. The string should contain valid XML,
    /// but validation is deferred to SQL Server.
    fn coerce_string_to_xml(py_obj: &Bound<'_, PyAny>) -> TdsResult<ColumnValues> {
        use mssql_tds::datatypes::column_values::SqlXml;

        let py_str = py_obj
            .cast::<PyString>()
            .map_err(|e| Error::UsageError(format!("Failed to cast to string: {}", e)))?;

        let xml_str = py_str
            .to_str()
            .map_err(|e| Error::UsageError(format!("Failed to extract string: {}", e)))?;

        // Convert string to SqlXml using the From<String> trait
        let sqlxml = SqlXml::from(xml_str.to_string());
        Ok(ColumnValues::Xml(sqlxml))
    }

    /// Coerce a Python list to a SQL Server VECTOR value.
    ///
    /// - Validates the target VECTOR base type is supported (Float32)
    /// - Ensures the Python list length matches expected dimensions from metadata
    /// - Converts elements to f32, rejecting NaN/Infinity
    fn coerce_list_to_vector(
        py_obj: &Bound<'_, PyAny>,
        target_meta: &BulkCopyColumnMetadata,
    ) -> TdsResult<ColumnValues> {
        // Validate base type support
        let base_type = VectorBaseType::try_from(target_meta.scale).map_err(|e| {
            Error::UsageError(format!(
                "Invalid VECTOR base type for column '{}': {}",
                target_meta.column_name, e
            ))
        })?;

        // Expected dimensions from metadata
        let expected_dims = target_meta.vector_dimensions()?;

        // Extract list and validate length
        let py_list = py_obj.cast::<pyo3::types::PyList>().map_err(|e| {
            Error::UsageError(format!(
                "Expected Python list for VECTOR column '{}': {}",
                target_meta.column_name, e
            ))
        })?;

        let seq_len = py_list.len();
        if seq_len != expected_dims {
            return Err(Error::UsageError(format!(
                "Vector dimension mismatch for column '{}': got {}, expected {}",
                target_meta.column_name, seq_len, expected_dims
            )));
        }

        match base_type {
            VectorBaseType::Float32 => {
                // Convert elements to f32 with validation (reject NaN/Inf)
                let mut values: Vec<f32> = Vec::with_capacity(seq_len);
                for (idx, item) in py_list.iter().enumerate() {
                    let as_f32 = item
                        .extract::<f32>()
                        .or_else(|_| item.extract::<i32>().map(|i| i as f32))
                        .map_err(|e| {
                            Error::UsageError(format!(
                                "VECTOR element conversion error at index {} in column '{}': {}",
                                idx, target_meta.column_name, e
                            ))
                        })?;

                    if !as_f32.is_finite() {
                        return Err(Error::UsageError(format!(
                            "VECTOR element at index {} in column '{}' is NaN or Infinity when converting to Float32",
                            idx, target_meta.column_name
                        )));
                    }

                    values.push(as_f32);
                }

                let vector = SqlVector::try_from_f32(values)?;
                Ok(ColumnValues::Vector(vector))
            }
            VectorBaseType::Float16 => Err(Error::UsageError(format!(
                "Float16 VECTOR column '{}' is not supported in Python bulk copy yet",
                target_meta.column_name
            ))),
        }
    }

    /// Coerce a Python string to a SQL Server VECTOR type by parsing JSON float array.
    /// Returns error if the string is not valid JSON or not a float array.
    fn coerce_string_to_vector(
        py_obj: &Bound<'_, PyAny>,
        target_meta: &BulkCopyColumnMetadata,
    ) -> TdsResult<ColumnValues> {
        // Validate base type support
        let base_type = VectorBaseType::try_from(target_meta.scale).map_err(|e| {
            Error::UsageError(format!(
                "Invalid VECTOR base type for column '{}': {}",
                target_meta.column_name, e
            ))
        })?;

        // Expected dimensions from metadata
        let expected_dims = target_meta.vector_dimensions()?;

        let py_str = py_obj.cast::<PyString>().map_err(|e| {
            Error::UsageError(format!(
                "Failed to cast to string for VECTOR column '{}': {}",
                target_meta.column_name, e
            ))
        })?;
        let s = py_str.to_str().map_err(|e| {
            Error::UsageError(format!(
                "Failed to extract string for VECTOR column '{}': {}",
                target_meta.column_name, e
            ))
        })?;

        // Parse JSON using serde_json and validate it's an array
        let json_value: serde_json::Value = serde_json::from_str(s).map_err(|e| {
            Error::UsageError(format!(
                "Invalid JSON string for VECTOR column '{}': {}",
                target_meta.column_name, e
            ))
        })?;

        let array = json_value.as_array().ok_or_else(|| {
            Error::UsageError(format!(
                "JSON is not an array for VECTOR column '{}'",
                target_meta.column_name
            ))
        })?;

        // Validate array length matches expected dimensions
        if array.len() != expected_dims {
            return Err(Error::UsageError(format!(
                "JSON array length {} does not match VECTOR({}) dimension for column '{}'",
                array.len(),
                expected_dims,
                target_meta.column_name
            )));
        }

        match base_type {
            VectorBaseType::Float32 => {
                // Extract f32 values from the array
                let mut floats = Vec::with_capacity(array.len());
                for (idx, item) in array.iter().enumerate() {
                    // serde_json only supports f64, so convert to f32
                    let f = item.as_f64()
                        .ok_or_else(|| Error::UsageError(format!(
                            "JSON array element at index {} could not be converted to float64 for VECTOR column '{}'", idx, target_meta.column_name
                        )))? as f32;

                    // Validate that the value is not NaN or Infinity
                    if !f.is_finite() {
                        return Err(Error::UsageError(format!(
                            "JSON array element at index {} is NaN or Infinity when converting to Float32 for VECTOR column '{}'",
                            idx, target_meta.column_name
                        )));
                    }

                    floats.push(f);
                }

                // Use SqlVector::try_from_f32, which returns Result<SqlVector, Error>
                let vector = SqlVector::try_from_f32(floats)?;
                Ok(ColumnValues::Vector(vector))
            }
            VectorBaseType::Float16 => Err(Error::UsageError(format!(
                "Float16 VECTOR column '{}' is not supported in Python bulk copy JSON coercion",
                target_meta.column_name
            ))),
        }
    }

    /// Coerce a Python string to a SQL Server UNIQUEIDENTIFIER type by parsing UUID string.
    ///
    /// Accepts standard UUID string formats:
    /// - Hyphenated: "6f9619ff-8b86-d011-b42d-00c04fc964ff"
    /// - Without hyphens: "6f9619ff8b86d011b42d00c04fc964ff"
    /// - Braced: "{6f9619ff-8b86-d011-b42d-00c04fc964ff}"
    /// - URN: "urn:uuid:6f9619ff-8b86-d011-b42d-00c04fc964ff"
    fn coerce_string_to_uuid(py_obj: &Bound<'_, PyAny>) -> TdsResult<ColumnValues> {
        let py_str = py_obj
            .cast::<PyString>()
            .map_err(|e| Error::UsageError(format!("Failed to cast to string: {}", e)))?;

        let s = py_str
            .to_str()
            .map_err(|e| Error::UsageError(format!("Failed to extract string: {}", e)))?;

        // Parse string as UUID using uuid::Uuid::try_parse which supports multiple formats
        let uuid = uuid::Uuid::try_parse(s)
            .map_err(|e| Error::UsageError(format!("Invalid UUID string '{}': {}", s, e)))?;

        Ok(ColumnValues::Uuid(uuid))
    }

    /// Coerce a Python integer to a SQL Server string type (NVARCHAR/VARCHAR/NCHAR/CHAR).
    ///
    /// Converts the integer to its string representation.
    fn coerce_int_to_string(py_obj: &Bound<'_, PyAny>) -> TdsResult<ColumnValues> {
        let value = py_obj
            .extract::<i64>()
            .map_err(|e| Error::UsageError(format!("Failed to extract integer: {}", e)))?;

        // Convert integer to string
        let string_value = value.to_string();

        // Create SqlString with UTF-16 encoding (standard for NVARCHAR)
        let sql_string =
            mssql_tds::datatypes::sql_string::SqlString::from_utf8_string(string_value);
        Ok(ColumnValues::String(sql_string))
    }

    /// Coerce a Python float to a SQL Server string type (NVARCHAR/VARCHAR/NCHAR/CHAR).
    ///
    /// Converts the float to its string representation.
    fn coerce_float_to_string(py_obj: &Bound<'_, PyAny>) -> TdsResult<ColumnValues> {
        let value = py_obj
            .extract::<f64>()
            .map_err(|e| Error::UsageError(format!("Failed to extract float: {}", e)))?;

        // Convert float to string
        let string_value = value.to_string();

        // Create SqlString with UTF-16 encoding (standard for NVARCHAR)
        let sql_string =
            mssql_tds::datatypes::sql_string::SqlString::from_utf8_string(string_value);
        Ok(ColumnValues::String(sql_string))
    }

    /// Coerce a Python boolean to a SQL Server string type (NVARCHAR/VARCHAR/NCHAR/CHAR).
    ///
    /// Converts boolean to 'True' or 'False' string (matching Python's str() behavior).
    fn coerce_bool_to_string(py_obj: &Bound<'_, PyAny>) -> TdsResult<ColumnValues> {
        let value = py_obj
            .extract::<bool>()
            .map_err(|e| Error::UsageError(format!("Failed to extract boolean: {}", e)))?;

        // Convert boolean to string ('True' or 'False')
        let string_value = if value { "True" } else { "False" }.to_string();

        // Create SqlString with UTF-16 encoding (standard for NVARCHAR)
        let sql_string =
            mssql_tds::datatypes::sql_string::SqlString::from_utf8_string(string_value);
        Ok(ColumnValues::String(sql_string))
    }

    /// Coerce a Python Decimal to a SQL Server string type (NVARCHAR/VARCHAR/NCHAR/CHAR).
    ///
    /// Converts the decimal to its string representation.
    fn coerce_decimal_to_string(py_obj: &Bound<'_, PyAny>) -> TdsResult<ColumnValues> {
        // Call str() on the Decimal object to get its string representation
        let string_value = py_obj
            .str()
            .and_then(|s| s.extract::<String>())
            .map_err(|e| {
                Error::UsageError(format!("Failed to convert Decimal to string: {}", e))
            })?;

        // Create SqlString with UTF-16 encoding (standard for NVARCHAR)
        let sql_string =
            mssql_tds::datatypes::sql_string::SqlString::from_utf8_string(string_value);
        Ok(ColumnValues::String(sql_string))
    }
}

#[async_trait]
impl BulkLoadRow for PythonRowAdapter {
    /// Write this row's column values directly to the TDS packet writer.
    ///
    /// This method attaches to Python to read each Python value, converts it
    /// to a ColumnValues variant, and writes it directly to the packet.
    ///
    /// The implementation strategy:
    /// 1. Inside `Python::attach`, convert all Python values to Vec<ColumnValues>
    /// 2. Leave the attach scope
    /// 3. Write each ColumnValues to packet asynchronously
    ///
    /// The temporary Vec is one allocation per row, consumed immediately, and
    /// nothing is buffered at the batch level. Leaving the attach scope does not
    /// release the GIL if the caller already holds it, as `Cursor::bulkcopy`
    /// does.
    ///
    /// # Arguments
    ///
    /// * `writer` - Streaming bulk load writer for direct packet serialization
    /// * `column_index` - Mutable column index tracker (maintained by writer)
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Python value cannot be converted to a TDS type
    /// - Column count doesn't match expected metadata
    /// - Network errors occur during transmission
    #[instrument(skip(self, writer), level = "debug")]
    async fn write_to_packet(
        &self,
        writer: &mut StreamingBulkLoadWriter<'_>,
        column_index: &mut usize,
    ) -> TdsResult<()> {
        // Step 1: Acquire GIL and convert Python values to ColumnValues
        let column_values: Vec<_> = Python::attach(|py| {
            let tuple = self.row.bind(py).cast::<PyTuple>().map_err(|e| {
                tracing::error!("Failed to cast row to tuple: {}", e);
                Error::UsageError(format!("Expected tuple, got: {}", e))
            })?;

            // If we have resolved mappings, use them to determine column order and indices
            if let Some(mappings) = &self.resolved_mappings {
                Self::validate_column_count(tuple.len(), self.expected_source_columns)?;

                // Use mappings to read columns in the correct order
                let mut values = Vec::with_capacity(mappings.len());

                for mapping in mappings.iter() {
                    // Read from source column index specified in the mapping
                    let item = tuple.get_item(mapping.source_index).map_err(|e| {
                        tracing::error!(
                            "Source column index {} out of bounds (tuple has {} columns): {}",
                            mapping.source_index,
                            tuple.len(),
                            e
                        );
                        Error::UsageError(format!(
                            "Source column index {} out of bounds (tuple has {} columns): {}",
                            mapping.source_index,
                            tuple.len(),
                            e
                        ))
                    })?;

                    // Get target metadata from destination_metadata using destination_index
                    let meta = self
                        .destination_metadata
                        .as_ref()
                        .and_then(|meta| meta.get(mapping.destination_index))
                        .ok_or_else(|| {
                            Error::UsageError(format!(
                                "Unexpected missing destination metadata for column index {}",
                                mapping.destination_index
                            ))
                        })?;

                    // Try conversion with type coercion and null validation
                    let column_value = Self::convert_by_target_type(&item, meta)?;

                    values.push(column_value);
                }

                Ok::<Vec<_>, Error>(values)
            } else {
                // No mappings - use sequential reading (original behavior)
                let mut values = Vec::with_capacity(tuple.len());

                for (i, item) in tuple.iter().enumerate() {
                    // Get target metadata if available
                    let meta = self
                        .destination_metadata
                        .as_ref()
                        .and_then(|meta| meta.get(i))
                        .ok_or_else(|| {
                            Error::UsageError(format!(
                                "Unexpected missing destination metadata for column index {}",
                                i
                            ))
                        })?;

                    // Try conversion with type coercion and null validation
                    let column_value = Self::convert_by_target_type(&item, meta)?;

                    values.push(column_value);
                }

                Ok::<Vec<_>, Error>(values)
            }
        })?;

        // Step 2: outside the attach scope, write values to packet asynchronously
        for value in column_values.iter() {
            writer.write_column_value(*column_index, value).await?;
            *column_index += 1;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn convert(value: &str, sql_type: SqlDbType, scale: u8) -> TdsResult<ColumnValues> {
        let metadata = BulkCopyColumnMetadata::new("event_time", sql_type, sql_type.to_tds_type())
            .with_scale(scale);
        Python::attach(|py| {
            PythonRowAdapter::convert_by_target_type(PyString::new(py, value).as_any(), &metadata)
        })
    }

    fn assert_temporal_value(input: &str, days: i64, ticks: i64, offset: i64, scale: u8) {
        for sql_type in [SqlDbType::DateTime2, SqlDbType::DateTimeOffset] {
            let actual = convert(input, sql_type, scale);
            if sql_type == SqlDbType::DateTimeOffset
                && (offset % 60 != 0 || offset.abs() > 14 * 3600)
            {
                assert!(actual.is_err(), "{input}");
                continue;
            }
            let (value, expected_ticks) = match actual
                .unwrap_or_else(|error| panic!("{input}: {error}"))
            {
                ColumnValues::DateTime2(value) if sql_type == SqlDbType::DateTime2 => {
                    (value, ticks)
                }
                ColumnValues::DateTimeOffset(value) if sql_type == SqlDbType::DateTimeOffset => {
                    assert_eq!(i64::from(value.offset) * 60, offset, "{input}");
                    (value.datetime2, ticks - offset * 10_000_000)
                }
                other => panic!("Unexpected value: {other:?}"),
            };
            assert_eq!(
                i64::from(value.days),
                days + expected_ticks.div_euclid(864_000_000_000),
                "{input}"
            );
            assert_eq!(
                value.time.time_nanoseconds as i64,
                expected_ticks.rem_euclid(864_000_000_000),
                "{input}"
            );
            assert_eq!(value.time.scale, scale, "{input}");
        }
    }

    #[test]
    fn datetime_strings_validate_calendar_dates() {
        for year in [1, 4, 100, 400, 1900, 2000, 2024, 9999] {
            for month in 1..=12 {
                for day in 1..=31 {
                    let value = format!("{year:04}-{month:02}-{day:02}T12:00:00.1234567");
                    assert_eq!(
                        parse_iso_datetime(&value).map(|(date, time, zone)| {
                            assert_eq!(time.hour(), 12);
                            assert_eq!(time.nanosecond(), 123_456_700);
                            assert_eq!(zone, "");
                            date
                        }),
                        NaiveDate::from_ymd_opt(year, month, day),
                        "{value}"
                    );
                }
            }
        }
    }

    #[test]
    fn compact_week_dates_preserve_clock_and_fraction() {
        for (input, ticks) in [
            ("2024-W27-120000", 432_000_000_000),
            ("2024-W27-120000.1234567", 432_001_234_567),
            ("2024-W27-1T120000.1234567", 432_001_234_567),
            ("2024W271T120000.1234567", 432_001_234_567),
            ("2024W271120000.1234567", 432_001_234_567),
            ("2024W27120000.1234567", 1_234_567),
            ("2024W271112:00:00.1234567", 432_001_234_567),
            ("2024-07-011120000.1234567", 432_001_234_567),
        ] {
            assert_temporal_value(input, 739_067, ticks, 0, 7);
        }
    }

    #[test]
    fn datetime_strings_preserve_legacy_fractional_forms() {
        for (clock, fractional_ticks) in [
            ("12000000", 0),
            ("12000012", 1_200_000),
            ("120000123", 1_230_000),
            ("1200001234", 1_234_000),
            ("12000012345", 1_234_500),
            ("120000123456", 1_234_560),
            ("1200001234567", 1_234_567),
            ("1200000000001", 1),
            ("1200009999999", 9_999_999),
            ("12:00:00:00", 0),
            ("12:00:00:1", 1_000_000),
            ("12:00:00:12", 1_200_000),
            ("12:00:00:123456", 1_234_560),
            ("12:00:00:1234567", 1_234_567),
            ("12:00:00:0000001", 1),
            ("12:00:00:9999999", 9_999_999),
        ] {
            for (zone, offset) in [("", 0), ("Z", 0), ("+05:30", 330), ("-07:00", -420)] {
                let input = format!("2024-07-01T{clock}{zone}");
                assert_temporal_value(
                    &input,
                    739_067,
                    432_000_000_000 + fractional_ticks,
                    offset * 60,
                    7,
                );
            }
        }
    }

    #[test]
    fn week_date_separators_match_python_datetime() {
        Python::attach(|py| {
            let datetime = py.import("datetime").unwrap().getattr("datetime").unwrap();
            // Python 3.10 has no week-date support; fixed-value regressions still run there.
            let supports_week_dates = datetime
                .call_method1("fromisoformat", ("2024-W27",))
                .is_ok();
            let mut checked = 0;
            for date in [
                "2024-07-01",
                "20240701",
                "2024-W27",
                "2024W27",
                "2024-W27-1",
                "2024W271",
                "2020-W53-7",
                "2020W537",
            ] {
                if date.contains('W') && !supports_week_dates {
                    continue;
                }
                for separator in ["T", " ", "-", "+", ":", "Z", "x", "\u{20ac}"] {
                    for hour in 0..24 {
                        for (clock, has_fraction) in [
                            (format!("{hour:02}"), false),
                            (format!("{hour:02}34"), false),
                            (format!("{hour:02}3456"), false),
                            (format!("{hour:02}345612"), true),
                            (format!("{hour:02}3456123456"), true),
                            (format!("{hour:02}:34"), false),
                            (format!("{hour:02}:34:56"), false),
                            (format!("{hour:02}:34:56:1"), true),
                            (format!("{hour:02}:34:56:123456"), true),
                        ] {
                            for fraction in ["", ".123456"] {
                                if has_fraction && !fraction.is_empty() {
                                    continue;
                                }
                                for zone in
                                    ["", "Z", "+05:30", "-07:00", "+05300000", "-07:00:00:0"]
                                {
                                    let input = format!("{date}{separator}{clock}{fraction}{zone}");
                                    let expected =
                                        datetime.call_method1("fromisoformat", (&input,));
                                    let Ok(expected) = &expected else {
                                        // Older Python also rejects valid compact clocks and Z.
                                        if supports_week_dates {
                                            for sql_type in
                                                [SqlDbType::DateTime2, SqlDbType::DateTimeOffset]
                                            {
                                                assert!(
                                                    convert(&input, sql_type, 7).is_err(),
                                                    "{input}"
                                                );
                                            }
                                        }
                                        continue;
                                    };
                                    let component = |name: &str| {
                                        expected.getattr(name).unwrap().extract::<i64>().unwrap()
                                    };
                                    let days = expected
                                        .call_method0("toordinal")
                                        .unwrap()
                                        .extract::<i64>()
                                        .unwrap()
                                        - 1;
                                    let ticks = component("hour") * 36_000_000_000
                                        + component("minute") * 600_000_000
                                        + component("second") * 10_000_000
                                        + component("microsecond") * 10;
                                    let offset = expected.call_method0("utcoffset").unwrap();
                                    let offset_seconds = if offset.is_none() {
                                        0
                                    } else {
                                        offset
                                            .call_method0("total_seconds")
                                            .unwrap()
                                            .extract::<f64>()
                                            .unwrap() as i64
                                    };
                                    assert_temporal_value(&input, days, ticks, offset_seconds, 7);
                                    checked += 2;
                                }
                            }
                        }
                    }
                }
            }
            assert!(
                checked > 1_000,
                "Compatibility comparisons must not be vacuous"
            );
        });
    }

    #[test]
    fn datetime_strings_preserve_seven_fractional_digits() {
        for (fraction, ticks) in [
            ("", 0),
            (".1", 1_000_000),
            (".12", 1_200_000),
            (".123", 1_230_000),
            (".1234", 1_234_000),
            (".12345", 1_234_500),
            (".123456", 1_234_560),
            (".1234567", 1_234_567),
            (".0000001", 1),
            (".9999999", 9_999_999),
        ] {
            for separator in [" ", "T"] {
                for zone in ["", "Z", "+00:00"] {
                    let input = format!("2024-07-01{separator}12:00:00{fraction}{zone}");
                    for scale in 0..=7 {
                        assert_temporal_value(&input, 739_067, 432_000_000_000 + ticks, 0, scale);
                    }
                }
            }
        }
    }

    #[test]
    fn datetimeoffset_strings_normalize_to_utc() {
        for (input, days, ticks, offset) in [
            (
                "2024-07-01T12:00:00.1234567+05:30",
                739_067,
                234_001_234_567,
                330,
            ),
            (
                "2024-07-01T12:00:00.1234567-07:00",
                739_067,
                684_001_234_567,
                -420,
            ),
            (
                "2024-07-01T00:00:00.0000001+14:00",
                739_066,
                360_000_000_001,
                840,
            ),
            (
                "2024-07-01T23:59:59.9999999-14:00",
                739_068,
                503_999_999_999,
                -840,
            ),
            ("0001-01-01T00:00:00Z", 0, 0, 0),
            (
                "9999-12-31T23:59:59.9999999Z",
                3_652_058,
                863_999_999_999,
                0,
            ),
        ] {
            let ColumnValues::DateTimeOffset(value) =
                convert(input, SqlDbType::DateTimeOffset, 7).unwrap()
            else {
                panic!("Expected DATETIMEOFFSET");
            };
            assert_eq!(value.datetime2.days, days, "{input}");
            assert_eq!(value.datetime2.time.time_nanoseconds, ticks, "{input}");
            assert_eq!(value.offset, offset, "{input}");
        }
    }

    #[test]
    fn datetime2_strings_keep_local_time_and_iso_variants() {
        for input in [
            "2024-7-1T12:00:00.1234567",
            "2024-07-01T12:00:00.1234567+05:30",
            "2024-07-01T12:00:00.1234567-07:00",
            "20240701T120000.1234567",
            "2024-W27-1T12:00:00,1234567",
            "2024W271T120000.1234567",
            "2024-W27T12:00:00.1234567",
            "2024-W27-12:00:00.1234567",
            "2024W27T120000.1234567",
            "2024-07-01T12:00:00.1234567+0530",
            "2024-07-01T12:00:00.1234567-07",
        ] {
            let ColumnValues::DateTime2(value) = convert(input, SqlDbType::DateTime2, 7).unwrap()
            else {
                panic!("Expected DATETIME2");
            };
            assert_eq!(value.days, 739_067);
            assert_eq!(value.time.time_nanoseconds, 432_001_234_567);
        }
        for (input, ticks) in [
            ("2024-07-01", 0),
            ("2024-W27", 0),
            ("2024W27", 0),
            ("2024-07-01T12", 432_000_000_000),
            ("2024-07-01T12:30", 450_000_000_000),
        ] {
            let ColumnValues::DateTime2(value) = convert(input, SqlDbType::DateTime2, 7).unwrap()
            else {
                panic!("Expected DATETIME2");
            };
            assert_eq!(value.time.time_nanoseconds, ticks);
        }
    }

    #[test]
    fn datetime_strings_normalize_offset_components() {
        for (zone, normalized) in [
            ("+05:60", "+06:00"),
            ("-05:60", "-06:00"),
            ("+00:00:60", "+00:01"),
            ("-00:00:60", "-00:01"),
            ("+00:90", "+01:30"),
            ("+0560", "+06:00"),
            ("-000060", "-00:01"),
            ("+13:59:60", "+14:00"),
            ("-13:60", "-14:00"),
        ] {
            for sql_type in [SqlDbType::DateTime2, SqlDbType::DateTimeOffset] {
                let input = format!("2024-07-01T00:00:00.1234567{zone}");
                let expected = format!("2024-07-01T00:00:00.1234567{normalized}");
                assert_eq!(
                    convert(&input, sql_type, 7).unwrap(),
                    convert(&expected, sql_type, 7).unwrap(),
                    "{input}"
                );
            }
        }
    }

    #[test]
    fn offset_component_ranges_match_python_datetime() {
        Python::attach(|py| {
            let datetime = py.import("datetime").unwrap().getattr("datetime").unwrap();
            for hour in [0, 5, 13, 14, 22, 23, 24, 99] {
                for minute in [0, 59, 60, 90, 99] {
                    for second in [0, 59, 60, 99] {
                        for sign in ["+", "-"] {
                            let input = format!(
                                "2024-07-01T12:00:00.1234567{sign}{hour:02}:{minute:02}:{second:02}"
                            );
                            // Python 3.10 rejects seven digits; this oracle only supplies the offset.
                            let reference = format!(
                                "2024-07-01T12:00:00.123456{sign}{hour:02}:{minute:02}:{second:02}"
                            );
                            let expected = datetime.call_method1("fromisoformat", (&reference,));
                            let Ok(expected) = &expected else {
                                for sql_type in [SqlDbType::DateTime2, SqlDbType::DateTimeOffset] {
                                    assert!(convert(&input, sql_type, 7).is_err(), "{input}");
                                }
                                continue;
                            };
                            let seconds = expected
                                .call_method0("utcoffset")
                                .unwrap()
                                .call_method0("total_seconds")
                                .unwrap()
                                .extract::<f64>()
                                .unwrap() as i64;
                            assert_temporal_value(&input, 739_067, 432_001_234_567, seconds, 7);
                        }
                    }
                }
            }
        });
    }

    #[test]
    fn datetime2_strings_ignore_subminute_offsets() {
        for offset in [
            "+00:00:30",
            "-00:00:30",
            "+00:00:00.1",
            "-00:00:30.1234567",
            "+00000012",
            "-0000301234567",
            "+00:00:00:1",
            "-00:00:30:1234567",
            "+00:99:99",
            "-00:00:60.1",
        ] {
            let input = format!("2024-07-01T00:00:00.1234567{offset}");
            let ColumnValues::DateTime2(value) = convert(&input, SqlDbType::DateTime2, 7).unwrap()
            else {
                panic!("Expected DATETIME2");
            };
            assert_eq!(value.days, 739_067, "{input}");
            assert_eq!(value.time.time_nanoseconds, 1_234_567, "{input}");
            assert_eq!(value.time.scale, 7);
            assert!(
                convert(&input, SqlDbType::DateTimeOffset, 7).is_err(),
                "{input}"
            );
        }
    }

    #[test]
    fn datetime_strings_reject_invalid_values() {
        for sql_type in [SqlDbType::DateTime2, SqlDbType::DateTimeOffset] {
            for input in [
                "",
                "2024-02-30T12:00:00",
                "0000-01-01",
                "10000-01-01",
                "2024-07-01T24:00:00",
                "2024-07-01T12:60:00",
                "2024-07-01T12:00:60",
                "2024-07-01T1",
                "2024-07-01T123",
                "2024-07-01T1200001",
                "2024-07-01T12000012345678",
                "2024-07-01T12000012.34",
                "2024-07-01T120000x12",
                "2024-07-01T12:00:00:",
                "2024-07-01T12:00:00:12:34",
                "2024-07-01T12:00:00:12.34",
                "2024-07-01T12:00:00:12345678",
                "2024-07-01T12:00:00.",
                "2024-07-01T12:00:00.12345678",
                "2024-07-01T12:00:00+24:00",
                "2024-07-01T12:00:00+23:59:60",
                "2024-07-01T12:00:00-23:60",
                "2024-07-01T12:00:00+00:100",
                "2024-07-01T12:00:00Z12",
                "2024-07-01T1:00:00",
                "2024-07-01T0\u{20ac}00",
                "2024-07-\u{20ac}T12:00:00",
                "\u{20ac}024-07-01T12:00:00",
                "2024-W27-1120000",
                "2024-W27-",
                "2024-W27-8T120000",
                "2024W278T120000",
                "2024-W54-1T120000",
                "2024-W27-\u{20ac}120000",
            ] {
                assert!(convert(input, sql_type, 7).is_err(), "{input}");
            }
        }
        for input in [
            "2024-07-01T12:00:00+14:01",
            "2024-07-01T12:00:00-14:01",
            "2024-07-01T12:00:00+13:60:60",
            "2024-07-01T12:00:00-14:60",
            "2024-07-01T12:00:00+00:00:30",
            "2024-07-01T12:00:00+00:00:00.1",
            "0001-01-01T00:00:00+00:01",
            "9999-12-31T23:59:59.9999999-00:01",
        ] {
            assert!(
                convert(input, SqlDbType::DateTimeOffset, 7).is_err(),
                "{input}"
            );
        }
    }
}
