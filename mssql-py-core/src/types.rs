// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Type conversion utilities between Python and SQL Server types

use mssql_tds::core::TdsResult;
use mssql_tds::datatypes::bulk_copy_metadata::{BulkCopyColumnMetadata, SqlDbType};
use mssql_tds::datatypes::column_values::ColumnValues;
use mssql_tds::datatypes::decoder::DecimalParts;
use mssql_tds::datatypes::sql_string::SqlString;
use mssql_tds::error::Error;
use pyo3::prelude::*;
use pyo3::types::{
    PyBool, PyByteArray, PyBytes, PyDate, PyDateTime, PyInt, PyModule, PyString, PyTime,
};

/// Convert a Python object to ColumnValues for TDS serialization
///
/// This function handles direct conversion from Python types to TDS column values,
/// supporting the most common SQL Server data types.
///
/// # Supported Types
///
/// - `None` → `ColumnValues::Null`
/// - `int` → `ColumnValues::Int` or `ColumnValues::BigInt`
/// - `float` → `ColumnValues::Float`
/// - `str` → `ColumnValues::String`
/// - `bool` → `ColumnValues::Bit`
/// - `bytes` → `ColumnValues::Binary`
/// - `datetime.datetime` → `ColumnValues::DateTime2`
/// - `datetime.date` → `ColumnValues::Date`
/// - `datetime.time` → `ColumnValues::Time`
///
/// # Arguments
///
/// * `py_obj` - Python object to convert
/// * `target_metadata` - Optional target column metadata for type validation
///
/// # Returns
///
/// `TdsResult<ColumnValues>` - The converted column value
///
/// # Errors
///
/// Returns an error if:
/// - The Python type is not supported or conversion fails
/// - When target_metadata is provided, if the converted type doesn't match the target SQL type
///
/// Fast-path converter that checks type once and extracts directly
///
/// This avoids the expensive fallback chain of trying bool→i32→i64→str
/// on every single value. Instead, we check the Python type name once
/// and use direct extraction.
///
/// # Type Validation
///
/// When `target_metadata` is provided, this function validates that the converted
/// ColumnValues type is compatible with the target SQL type. This prevents silent
/// type mismatches that could occur when try_type_coercion() returns None but
/// the type mapping isn't properly maintained.
pub fn py_to_column_value(
    py_obj: &Bound<'_, PyAny>,
    target_metadata: Option<&BulkCopyColumnMetadata>,
) -> TdsResult<ColumnValues> {
    let result = py_to_column_value_internal(py_obj, target_metadata)?;

    // Validate type compatibility if metadata provided
    if let Some(meta) = target_metadata {
        validate_type_compatibility(&result, meta)?;
    }

    Ok(result)
}

/// Internal conversion function without validation.
///
/// This is the core type conversion logic extracted to a separate function
/// to keep the validation step clean and maintainable.
fn py_to_column_value_internal(
    py_obj: &Bound<'_, PyAny>,
    target_metadata: Option<&BulkCopyColumnMetadata>,
) -> TdsResult<ColumnValues> {
    // Handle None (NULL) - most common check
    if py_obj.is_none() {
        return Ok(ColumnValues::Null);
    }

    // Fast path: check instance type directly
    // This is much faster than trying extract::<T>() in sequence

    // Check for bool FIRST (before int check)
    // Important: In Python, bool is a subclass of int, so isinstance(True, int) returns True.
    // We must check for bool before int to ensure booleans map to Bit instead of Int.
    if py_obj.is_instance_of::<PyBool>() {
        let val = py_obj
            .extract::<bool>()
            .map_err(|e| Error::UsageError(format!("Failed to extract bool: {}", e)))?;
        return Ok(ColumnValues::Bit(val));
    }

    // Check for int (most common in bulk copy)
    if py_obj.is_instance_of::<PyInt>() {
        // Try i32 first (most common range)
        if let Ok(val) = py_obj.extract::<i32>() {
            return Ok(ColumnValues::Int(val));
        }
        // Fallback to i64 for large integers
        if let Ok(val) = py_obj.extract::<i64>() {
            return Ok(ColumnValues::BigInt(val));
        }
    }

    // Check for string (second most common)
    if py_obj.is_instance_of::<PyString>() {
        // Direct string extraction - no fallback needed
        let val = py_obj
            .extract::<String>()
            .map_err(|e| Error::UsageError(format!("Failed to extract string: {}", e)))?;
        let sql_string = SqlString::from_utf8_string(val);
        return Ok(ColumnValues::String(sql_string));
    }

    // Check for float
    if py_obj.is_exact_instance_of::<pyo3::types::PyFloat>() {
        let val = py_obj
            .extract::<f64>()
            .map_err(|e| Error::UsageError(format!("Failed to extract float: {}", e)))?;

        // Check if target metadata specifies REAL vs FLOAT
        if let Some(meta) = target_metadata {
            match meta.sql_type {
                SqlDbType::Real => {
                    // Convert to Real (f32) - may lose precision
                    return Ok(ColumnValues::Real(val as f32));
                }
                SqlDbType::Float => {
                    // Keep as Float (f64)
                    return Ok(ColumnValues::Float(val));
                }
                _ => {
                    // For other target types, use Float and let coercion handle it
                    return Ok(ColumnValues::Float(val));
                }
            }
        }

        // No metadata provided - default to Float (f64)
        return Ok(ColumnValues::Float(val));
    }

    // Check for bytes
    if py_obj.is_instance_of::<PyBytes>() {
        let bytes = py_obj
            .extract::<Vec<u8>>()
            .map_err(|e| Error::UsageError(format!("Failed to extract bytes: {}", e)))?;
        return Ok(ColumnValues::Bytes(bytes));
    }

    // Check for bytearray (mutable bytes)
    if py_obj.is_instance_of::<PyByteArray>() {
        let bytes = py_obj
            .extract::<Vec<u8>>()
            .map_err(|e| Error::UsageError(format!("Failed to extract bytearray: {}", e)))?;
        return Ok(ColumnValues::Bytes(bytes));
    }

    // Check for datetime types (must check PyDateTime before PyDate since datetime is a subclass of date)
    if py_obj.is_instance_of::<PyDateTime>() {
        // Extract components from Python datetime
        let year = py_obj
            .getattr("year")
            .and_then(|v| v.extract::<i32>())
            .map_err(|e| Error::UsageError(format!("Failed to get year from datetime: {}", e)))?;

        let month = py_obj
            .getattr("month")
            .and_then(|v| v.extract::<u8>())
            .map_err(|e| Error::UsageError(format!("Failed to get month from datetime: {}", e)))?;

        let day = py_obj
            .getattr("day")
            .and_then(|v| v.extract::<u8>())
            .map_err(|e| Error::UsageError(format!("Failed to get day from datetime: {}", e)))?;

        let hour = py_obj
            .getattr("hour")
            .and_then(|v| v.extract::<u8>())
            .map_err(|e| Error::UsageError(format!("Failed to get hour from datetime: {}", e)))?;

        let minute = py_obj
            .getattr("minute")
            .and_then(|v| v.extract::<u8>())
            .map_err(|e| Error::UsageError(format!("Failed to get minute from datetime: {}", e)))?;

        let second = py_obj
            .getattr("second")
            .and_then(|v| v.extract::<u8>())
            .map_err(|e| Error::UsageError(format!("Failed to get second from datetime: {}", e)))?;

        let microsecond = py_obj
            .getattr("microsecond")
            .and_then(|v| v.extract::<u32>())
            .map_err(|e| {
                Error::UsageError(format!("Failed to get microsecond from datetime: {}", e))
            })?;

        // Calculate days since 1900-01-01
        // Use Python's date.toordinal() which gives days since 0001-01-01 (1-based)
        // Then subtract the ordinal of 1900-01-01
        let py = py_obj.py();
        let datetime_module = PyModule::import(py, "datetime")
            .map_err(|e| Error::UsageError(format!("Failed to import datetime module: {}", e)))?;

        let date_class = datetime_module
            .getattr("date")
            .map_err(|e| Error::UsageError(format!("Failed to get date class: {}", e)))?;

        // Create date for 1900-01-01
        let base_date = date_class
            .call1((1900, 1, 1))
            .map_err(|e| Error::UsageError(format!("Failed to create base date: {}", e)))?;

        let base_ordinal = base_date
            .call_method0("toordinal")
            .and_then(|v| v.extract::<i32>())
            .map_err(|e| Error::UsageError(format!("Failed to get base ordinal: {}", e)))?;

        // Get ordinal of the current datetime's date part
        let current_date = date_class
            .call1((year, month, day))
            .map_err(|e| Error::UsageError(format!("Failed to create current date: {}", e)))?;

        let current_ordinal = current_date
            .call_method0("toordinal")
            .and_then(|v| v.extract::<i32>())
            .map_err(|e| Error::UsageError(format!("Failed to get current ordinal: {}", e)))?;

        let days = current_ordinal - base_ordinal;

        // Check if target is DateTimeOffset, SmallDateTime or DateTime2 to determine which format to use
        if let Some(meta) = target_metadata {
            if meta.sql_type == SqlDbType::DateTimeOffset {
                // Convert to DATETIMEOFFSET format
                // DATETIMEOFFSET uses DATETIME2 + timezone offset (i16, minutes from UTC)
                // Calculate days from year 1 using Python's toordinal
                let current_ordinal = current_date
                    .call_method0("toordinal")
                    .and_then(|v| v.extract::<u32>())
                    .map_err(|e| {
                        Error::UsageError(format!(
                            "Failed to get current ordinal for DATETIMEOFFSET: {}",
                            e
                        ))
                    })?;

                // Python's toordinal() returns 1 for 0001-01-01, so subtract 1 to get 0-based days
                let days_dto = current_ordinal.checked_sub(1).ok_or_else(|| {
                    Error::UsageError(
                        "Date ordinal is 0, expected >= 1 for DATETIMEOFFSET".to_string(),
                    )
                })?;

                // Convert to 100-nanosecond units (DATETIME2/TIME uses 100ns precision)
                let time_nanoseconds = (hour as u64) * 36_000_000_000
                    + (minute as u64) * 600_000_000
                    + (second as u64) * 10_000_000
                    + (microsecond as u64) * 10;

                // Use the scale from metadata, defaulting to 7 (max precision)
                let scale = meta.scale;

                // Extract timezone offset
                let offset_minutes = match py_obj.call_method0("utcoffset") {
                    Ok(offset_delta) if !offset_delta.is_none() => {
                        // Get offset in seconds and convert to minutes
                        let offset_seconds = offset_delta
                            .call_method0("total_seconds")
                            .and_then(|v| v.extract::<f64>())
                            .map_err(|e| {
                                Error::UsageError(format!(
                                    "Failed to get timezone offset seconds: {}",
                                    e
                                ))
                            })?;
                        (offset_seconds / 60.0).round() as i16
                    }
                    _ => {
                        // No timezone info, default to UTC (0 offset)
                        0
                    }
                };

                // Validate offset range: -840 to +840 minutes (-14:00 to +14:00)
                if !(-840..=840).contains(&offset_minutes) {
                    return Err(Error::UsageError(format!(
                        "Timezone offset {} minutes out of valid range for DATETIMEOFFSET (-840 to +840)",
                        offset_minutes
                    )));
                }

                return Ok(ColumnValues::DateTimeOffset(
                    mssql_tds::datatypes::column_values::SqlDateTimeOffset {
                        datetime2: mssql_tds::datatypes::column_values::SqlDateTime2 {
                            days: days_dto,
                            time: mssql_tds::datatypes::column_values::SqlTime {
                                time_nanoseconds,
                                scale,
                            },
                        },
                        offset: offset_minutes,
                    },
                ));
            } else if meta.sql_type == SqlDbType::DateTime2 {
                // Convert to DATETIME2 format
                // DATETIME2 uses days since 0001-01-01 (0-based) instead of days since 1900-01-01
                // Calculate days from year 1 using Python's toordinal
                let current_ordinal = current_date
                    .call_method0("toordinal")
                    .and_then(|v| v.extract::<u32>())
                    .map_err(|e| {
                        Error::UsageError(format!(
                            "Failed to get current ordinal for DATETIME2: {}",
                            e
                        ))
                    })?;

                // Python's toordinal() returns 1 for 0001-01-01, so subtract 1 to get 0-based days
                let days_dt2 = current_ordinal.checked_sub(1).ok_or_else(|| {
                    Error::UsageError("Date ordinal is 0, expected >= 1 for DATETIME2".to_string())
                })?;

                // Convert to 100-nanosecond units (DATETIME2/TIME uses 100ns precision)
                let time_nanoseconds = (hour as u64) * 36_000_000_000
                    + (minute as u64) * 600_000_000
                    + (second as u64) * 10_000_000
                    + (microsecond as u64) * 10;

                // Use the scale from metadata, defaulting to 7 (max precision)
                let scale = meta.scale;

                return Ok(ColumnValues::DateTime2(
                    mssql_tds::datatypes::column_values::SqlDateTime2 {
                        days: days_dt2,
                        time: mssql_tds::datatypes::column_values::SqlTime {
                            time_nanoseconds,
                            scale,
                        },
                    },
                ));
            } else if meta.sql_type == SqlDbType::SmallDateTime {
                // Validate SMALLDATETIME range: 1900-01-01 00:00:00 to 2079-06-06 23:59:59
                if !(0..=65535).contains(&days) {
                    return Err(Error::UsageError(format!(
                        "DateTime value {}-{:02}-{:02} out of range for SMALLDATETIME column '{}' (valid range: 1900-01-01 to 2079-06-06)",
                        year, month, day, meta.column_name
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
                        "DateTime value {}-{:02}-{:02} {hour:02}:{minute:02}:{second:02} out of range for SMALLDATETIME column '{}' after rounding (valid range: 1900-01-01 to 2079-06-06)",
                        year, month, day, meta.column_name
                    )));
                }

                let time_minutes = (rounded_hour as u16) * 60 + (rounded_minute as u16);

                return Ok(ColumnValues::SmallDateTime(
                    mssql_tds::datatypes::column_values::SqlSmallDateTime {
                        days: rounded_days as u16,
                        time: time_minutes,
                    },
                ));
            }
        }

        // Default to DATETIME format
        let (final_days, time_ticks) = datetime_to_ticks(days, hour, minute, second, microsecond)?;

        return Ok(ColumnValues::DateTime(
            mssql_tds::datatypes::column_values::SqlDateTime {
                days: final_days,
                time: time_ticks,
            },
        ));
    }

    if py_obj.is_instance_of::<PyDate>() {
        // Convert Python date object to SqlDate
        // Python's toordinal() is 1-based (date(1,1,1).toordinal() == 1)
        // SQL Server DATE needs 0-based days since 0001-01-01, so subtract 1
        match py_obj.call_method0("toordinal") {
            Ok(ordinal_obj) => {
                if let Ok(ordinal) = ordinal_obj.extract::<u32>() {
                    // Convert from 1-based ordinal to 0-based days
                    let days = ordinal.checked_sub(1).ok_or_else(|| {
                        Error::UsageError("Date ordinal is 0, expected >= 1".to_string())
                    })?;
                    return Ok(ColumnValues::Date(
                        mssql_tds::datatypes::column_values::SqlDate::create(days)?,
                    ));
                }
            }
            Err(e) => {
                return Err(Error::UsageError(format!(
                    "Failed to get ordinal from date object: {}",
                    e
                )));
            }
        }
    }

    if py_obj.is_instance_of::<PyTime>() {
        // Convert Python time object to SqlTime
        // Extract hour, minute, second, microsecond from Python time
        let hour = py_obj
            .getattr("hour")
            .and_then(|v| v.extract::<u8>())
            .map_err(|e| Error::UsageError(format!("Failed to get hour from time: {}", e)))?;

        let minute = py_obj
            .getattr("minute")
            .and_then(|v| v.extract::<u8>())
            .map_err(|e| Error::UsageError(format!("Failed to get minute from time: {}", e)))?;

        let second = py_obj
            .getattr("second")
            .and_then(|v| v.extract::<u8>())
            .map_err(|e| Error::UsageError(format!("Failed to get second from time: {}", e)))?;

        let microsecond = py_obj
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

        // Use the scale from target metadata if available, otherwise default to 7 (max precision)
        let scale = target_metadata.map(|meta| meta.scale).unwrap_or(7);

        return Ok(ColumnValues::Time(
            mssql_tds::datatypes::column_values::SqlTime {
                time_nanoseconds,
                scale,
            },
        ));
    }

    // Check for decimal.Decimal type
    // We need to check if the object is an instance of decimal.Decimal
    let py = py_obj.py();
    if let Ok(decimal_module) = PyModule::import(py, "decimal")
        && let Ok(decimal_class) = decimal_module.getattr("Decimal")
        && let Ok(is_instance) = py_obj.is_instance(&decimal_class)
        && is_instance
    {
        // Extract Decimal as string and parse it
        if let Ok(decimal_str) = py_obj.call_method0("__str__")
            && let Ok(s) = decimal_str.extract::<String>()
        {
            // Use a reasonable precision and scale for default conversion
            // This will be validated/adjusted during bulk copy if metadata is available
            match DecimalParts::from_string(&s, 38, 10) {
                Ok(decimal_parts) => return Ok(ColumnValues::Decimal(decimal_parts)),
                Err(e) => {
                    return Err(Error::UsageError(format!(
                        "Failed to convert Python Decimal '{}': {}",
                        s, e
                    )));
                }
            }
        }
        return Err(Error::UsageError(
            "Failed to extract Decimal value as string".to_string(),
        ));
    }

    // Check for uuid.UUID type
    // Python's UUID type from the uuid module
    if let Ok(uuid_module) = PyModule::import(py, "uuid")
        && let Ok(uuid_class) = uuid_module.getattr("UUID")
        && let Ok(is_instance) = py_obj.is_instance(&uuid_class)
        && is_instance
    {
        // Extract UUID bytes (16 bytes in big-endian RFC 4122 format)
        // Python's UUID.bytes property returns bytes in big-endian order
        let bytes_obj = py_obj.getattr("bytes").map_err(|e| {
            Error::UsageError(format!(
                "Failed to get 'bytes' attribute from Python UUID object: {}",
                e
            ))
        })?;

        let uuid_bytes = bytes_obj.extract::<Vec<u8>>().map_err(|e| {
            Error::UsageError(format!(
                "Failed to extract bytes from Python UUID.bytes property: {}",
                e
            ))
        })?;

        if uuid_bytes.len() != 16 {
            return Err(Error::UsageError(format!(
                "Invalid UUID byte length: expected 16, got {}",
                uuid_bytes.len()
            )));
        }

        // Convert Python UUID bytes to Rust uuid::Uuid
        // Python's UUID.bytes is in RFC 4122 big-endian format
        let mut uuid_array = [0u8; 16];
        uuid_array.copy_from_slice(&uuid_bytes);
        let rust_uuid = uuid::Uuid::from_bytes(uuid_array);
        return Ok(ColumnValues::Uuid(rust_uuid));
    }

    // Unsupported type
    let type_name = py_obj
        .get_type()
        .name()
        .map(|n| n.to_string())
        .unwrap_or_else(|_| "<unknown>".to_string());

    Err(Error::UsageError(format!(
        "Unsupported Python type for bulk copy: {}",
        type_name
    )))
}

/// Ticks-per-day for SQL Server DATETIME (300 ticks/sec × 86400 sec/day).
const DATETIME_TICKS_PER_DAY: u64 = 25_920_000;

/// Convert time components to SQL Server DATETIME days + 1/300s ticks.
///
/// Rounds to the nearest tick (matching SqlClient's `SqlDateTime` behavior)
/// and normalizes midnight carry into the next day.
pub(crate) fn datetime_to_ticks(
    days: i32,
    hour: u8,
    minute: u8,
    second: u8,
    microsecond: u32,
) -> TdsResult<(i32, u32)> {
    let total_us = (hour as u64) * 3_600_000_000
        + (minute as u64) * 60_000_000
        + (second as u64) * 1_000_000
        + (microsecond as u64);

    // Round to nearest 1/300s tick (+ 500_000 implements round-half-up in integer math)
    let mut time_ticks = ((total_us * 300 + 500_000) / 1_000_000) as u32;
    let mut final_days = days;

    // Normalize midnight carry (rounding can push 23:59:59.998334+ to next day)
    if time_ticks as u64 >= DATETIME_TICKS_PER_DAY {
        final_days += (time_ticks as u64 / DATETIME_TICKS_PER_DAY) as i32;
        time_ticks = (time_ticks as u64 % DATETIME_TICKS_PER_DAY) as u32;
    }

    // DATETIME range: 1753-01-01 (days = -53690) to 9999-12-31 (days = 2958463)
    if !(-53690..=2958463).contains(&final_days) {
        return Err(Error::UsageError(format!(
            "DATETIME value out of range after rounding (days={final_days}). \
             Valid range: 1753-01-01 to 9999-12-31."
        )));
    }

    Ok((final_days, time_ticks))
}

/// Convert SQL Server DATETIME 1/300s ticks to (hour, minute, second, microsecond).
///
/// Rounds to the nearest representable Python microsecond.
/// Clamps out-of-range ticks to 23:59:59.999999 to prevent silent `as u8` truncation
/// on malformed wire data.
pub(crate) fn ticks_to_time_components(time_ticks: u32) -> (u8, u8, u8, u32) {
    // Max valid ticks = 25_919_999 (23:59:59 + 299/300 s).
    // Clamp to prevent u8 overflow from corrupt data.
    let clamped = (time_ticks as u64).min(DATETIME_TICKS_PER_DAY - 1);

    // Round to nearest microsecond (+ 150 implements round-half-up for /300)
    let total_us = (clamped * 1_000_000 + 150) / 300;

    let hour = (total_us / 3_600_000_000) as u8;
    let remainder = total_us % 3_600_000_000;
    let minute = (remainder / 60_000_000) as u8;
    let remainder = remainder % 60_000_000;
    let second = (remainder / 1_000_000) as u8;
    let microsecond = (remainder % 1_000_000) as u32;

    (hour, minute, second, microsecond)
}

/// Validate that the converted ColumnValues type matches the target SQL type.
///
/// This function ensures type safety by verifying that the result of py_to_column_value()
/// is compatible with the target column metadata provided. This prevents silent type
/// mismatches that could occur if try_type_coercion() incorrectly returns None.
///
/// # Arguments
///
/// * `result` - The ColumnValues produced by conversion
/// * `target_metadata` - The target column metadata to validate against
///
/// # Returns
///
/// `TdsResult<()>` - Ok if types are compatible, Err with descriptive message otherwise
fn validate_type_compatibility(
    result: &ColumnValues,
    target_metadata: &BulkCopyColumnMetadata,
) -> TdsResult<()> {
    // Check if result type matches target type
    let result_matches_target = match (&result, target_metadata.sql_type) {
        // Integer types
        (ColumnValues::TinyInt(_), SqlDbType::TinyInt) => true,
        (ColumnValues::SmallInt(_), SqlDbType::SmallInt) => true,
        (ColumnValues::Int(_), SqlDbType::Int) => true,
        (ColumnValues::BigInt(_), SqlDbType::BigInt) => true,

        // Float/Decimal
        (ColumnValues::Float(_), SqlDbType::Float) => true,
        (ColumnValues::Real(_), SqlDbType::Real) => true,
        (ColumnValues::Numeric(_), SqlDbType::Numeric | SqlDbType::Decimal) => true,

        // String
        (
            ColumnValues::String(_),
            SqlDbType::VarChar
            | SqlDbType::NVarChar
            | SqlDbType::Char
            | SqlDbType::NChar
            | SqlDbType::Text
            | SqlDbType::NText,
        ) => true,

        // Binary (including legacy IMAGE type)
        (ColumnValues::Bytes(_), SqlDbType::VarBinary | SqlDbType::Binary | SqlDbType::Image) => {
            true
        }

        // Boolean
        (ColumnValues::Bit(_), SqlDbType::Bit) => true,

        // Date/Time
        (ColumnValues::Date(_), SqlDbType::Date) => true,
        (ColumnValues::DateTime2(_), SqlDbType::DateTime2) => true,
        (ColumnValues::DateTimeOffset(_), SqlDbType::DateTimeOffset) => true,
        (ColumnValues::DateTime(_), SqlDbType::DateTime) => true,
        (ColumnValues::SmallDateTime(_), SqlDbType::SmallDateTime) => true,
        (ColumnValues::Time(_), SqlDbType::Time) => true,

        // Money
        (ColumnValues::Money(_), SqlDbType::Money) => true,
        (ColumnValues::SmallMoney(_), SqlDbType::SmallMoney) => true,

        // UUID/GUID
        (ColumnValues::Uuid(_), SqlDbType::UniqueIdentifier) => true,

        // JSON
        (ColumnValues::Json(_), SqlDbType::Json) => true,

        // Vector
        (ColumnValues::Vector(_), SqlDbType::Vector) => true,

        // Variant - can hold most types except text, ntext, image, timestamp, sql_variant, vector, xml, json
        // Note: text/ntext/image don't have dedicated ColumnValues variants (use String/Bytes)
        (_, SqlDbType::Variant) => {
            // Check if the type is NOT one of the unsupported types in sql_variant
            !matches!(
                result,
                ColumnValues::Xml(_) | ColumnValues::Json(_) | ColumnValues::Vector(_)
            )
        }

        // NULL is always compatible
        (ColumnValues::Null, _) => true,

        // No match - type mismatch
        _ => false,
    };

    if !result_matches_target {
        return Err(Error::UsageError(format!(
            "Type mismatch for column '{}': converted to {:?} but target SQL type is {:?}. \
             This indicates try_type_coercion() returned None for an incompatible type pair.",
            target_metadata.column_name, result, target_metadata.sql_type
        )));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn datetime_to_ticks_rounds_up() {
        // 123ms = 0.123s → 0.123 * 300 = 36.9 ticks → rounds to 37
        let (days, ticks) = datetime_to_ticks(0, 0, 0, 0, 123_000).unwrap();
        assert_eq!(days, 0);
        assert_eq!(ticks, 37); // 37/300 = 0.12333...s matches SQL .1233333
    }

    #[test]
    fn datetime_to_ticks_rounds_down() {
        // 1666µs → 1666 * 300 / 1_000_000 = 0.4998 → rounds to 0
        let (_, ticks) = datetime_to_ticks(0, 0, 0, 0, 1_666).unwrap();
        assert_eq!(ticks, 0);
    }

    #[test]
    fn datetime_to_ticks_rounds_at_boundary() {
        // 1667µs → 1667 * 300 + 500_000 = 1_000_100 → / 1_000_000 = 1
        let (_, ticks) = datetime_to_ticks(0, 0, 0, 0, 1_667).unwrap();
        assert_eq!(ticks, 1);
    }

    #[test]
    fn datetime_to_ticks_half_tick_rounds_up() {
        // 5000µs = exactly 1.5 ticks → rounds to 2
        let (_, ticks) = datetime_to_ticks(0, 0, 0, 0, 5_000).unwrap();
        assert_eq!(ticks, 2);
    }

    #[test]
    fn datetime_to_ticks_full_second() {
        // 0µs at second boundary → exactly 300 ticks per second
        let (_, ticks) = datetime_to_ticks(0, 0, 0, 1, 0).unwrap();
        assert_eq!(ticks, 300);
    }

    #[test]
    fn datetime_to_ticks_midnight_carry() {
        // 23:59:59.999999 → rounds past midnight, should increment day
        let (days, ticks) = datetime_to_ticks(100, 23, 59, 59, 999_999).unwrap();
        assert_eq!(days, 101);
        assert_eq!(ticks, 0);
    }

    #[test]
    fn datetime_to_ticks_max_date_overflow() {
        // 9999-12-31 23:59:59.999999 → carry pushes to day 2958464, out of range
        let result = datetime_to_ticks(2958463, 23, 59, 59, 999_999);
        assert!(result.is_err());
    }

    #[test]
    fn datetime_to_ticks_no_carry_before_threshold() {
        // 23:59:59.996666 → ticks = 25_919_999, no carry
        let (days, ticks) = datetime_to_ticks(100, 23, 59, 59, 996_666).unwrap();
        assert_eq!(days, 100);
        assert_eq!(ticks, 25_919_999);
    }

    #[test]
    fn ticks_to_time_preserves_sub_ms() {
        // Tick 37 = 37/300 s = 0.12333...s = 123333.33µs → rounds to 123333µs
        let (h, m, s, us) = ticks_to_time_components(37);
        assert_eq!((h, m, s), (0, 0, 0));
        assert_eq!(us, 123_333);
    }

    #[test]
    fn ticks_to_time_zero() {
        let (h, m, s, us) = ticks_to_time_components(0);
        assert_eq!((h, m, s, us), (0, 0, 0, 0));
    }

    #[test]
    fn ticks_to_time_full_day() {
        // 25_919_999 ticks = 23:59:59.996666...
        let (h, m, s, us) = ticks_to_time_components(25_919_999);
        assert_eq!(h, 23);
        assert_eq!(m, 59);
        assert_eq!(s, 59);
        // 25_919_999 * 1_000_000 + 150 / 300 = 86_399_996_667µs total
        // remainder after 23:59:59 = 996_667µs
        assert_eq!(us, 996_667);
    }

    #[test]
    fn ticks_to_time_clamps_overflow() {
        // Out-of-range ticks should clamp to 23:59:59 instead of wrapping u8
        let (h, m, s, _) = ticks_to_time_components(u32::MAX);
        assert_eq!(h, 23);
        assert_eq!(m, 59);
        assert_eq!(s, 59);
    }

    #[test]
    fn roundtrip_encode_decode_preserves_value() {
        // Encode 123000µs → tick 37 → decode → 123333µs
        // Sub-ms precision is gained on decode because one tick spans ~3333µs
        let (_, ticks) = datetime_to_ticks(0, 16, 33, 33, 123_000).unwrap();
        assert_eq!(ticks, 17_883_937);

        let (h, m, s, us) = ticks_to_time_components(ticks);
        assert_eq!((h, m, s), (16, 33, 33));
        assert_eq!(us, 123_333);
    }
}
