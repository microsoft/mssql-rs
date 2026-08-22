// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

// Type conversion utilities between Python and SQL Server types

use mssql_tds::core::TdsResult;
use mssql_tds::datatypes::bulk_copy_metadata::{BulkCopyColumnMetadata, SqlDbType};
use mssql_tds::datatypes::column_values::{
    ColumnValues, SqlDateTime2, SqlDateTimeOffset, SqlMoney, SqlSmallMoney, SqlTime, SqlXml,
};
use mssql_tds::datatypes::decoder::DecimalParts;
use mssql_tds::datatypes::sql_json::SqlJson;
use mssql_tds::datatypes::sql_string::SqlString;
use mssql_tds::datatypes::sql_vector::SqlVector;
use mssql_tds::datatypes::sqltypes::SqlType;
use mssql_tds::error::Error;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::{
    PyBool, PyByteArray, PyBytes, PyDate, PyDateTime, PyFloat, PyInt, PyModule, PyString, PyTime,
    PyType,
};

static DECIMAL_TYPE: PyOnceLock<Py<PyType>> = PyOnceLock::new();
static UUID_TYPE: PyOnceLock<Py<PyType>> = PyOnceLock::new();
static JSON_DUMPS: PyOnceLock<Py<PyAny>> = PyOnceLock::new();

fn decimal_type(py: Python<'_>) -> PyResult<&Bound<'_, PyType>> {
    DECIMAL_TYPE.import(py, "decimal", "Decimal")
}

fn uuid_type(py: Python<'_>) -> PyResult<&Bound<'_, PyType>> {
    UUID_TYPE.import(py, "uuid", "UUID")
}

/// A supported ODBC `setinputsizes()` type after boundary validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum InputSqlType {
    Bit,
    TinyInt,
    SmallInt,
    Integer,
    BigInt,
    Real,
    Float,
    Numeric,
    Decimal,
    Char,
    VarChar,
    LongVarChar,
    WChar,
    WVarChar,
    WLongVarChar,
    Binary,
    VarBinary,
    LongVarBinary,
    Date,
    Time,
    DateTime,
    DateTimeOffset,
    Guid,
    Xml,
    Json,
    Vector,
    Money,
    SmallMoney,
    Variant,
    Udt,
}

impl TryFrom<i32> for InputSqlType {
    type Error = Error;

    fn try_from(value: i32) -> Result<Self, Self::Error> {
        Ok(match value {
            -7 => Self::Bit,
            -6 => Self::TinyInt,
            5 => Self::SmallInt,
            4 => Self::Integer,
            -5 => Self::BigInt,
            7 => Self::Real,
            6 | 8 => Self::Float,
            2 => Self::Numeric,
            3 => Self::Decimal,
            1 => Self::Char,
            12 => Self::VarChar,
            -1 => Self::LongVarChar,
            -8 => Self::WChar,
            -9 => Self::WVarChar,
            -10 => Self::WLongVarChar,
            -2 => Self::Binary,
            -3 => Self::VarBinary,
            -4 => Self::LongVarBinary,
            9 | 91 => Self::Date,
            10 | 92 | -154 => Self::Time,
            11 | 93 => Self::DateTime,
            -155 => Self::DateTimeOffset,
            -11 => Self::Guid,
            -152 | 241 => Self::Xml,
            244 => Self::Json,
            245 => Self::Vector,
            60 => Self::Money,
            122 => Self::SmallMoney,
            -150 => Self::Variant,
            -151 => Self::Udt,
            _ => {
                return Err(Error::UsageError(format!(
                    "Invalid SQL type: {value}. Must be a supported SQL type constant"
                )));
            }
        })
    }
}

/// A validated ODBC-compatible `setinputsizes()` entry lowered to [`SqlType`]
/// before TDS serialization.
#[derive(Clone, Copy)]
pub(crate) struct ParameterHint {
    sql_type: InputSqlType,
    /// Precision, dimension count, or character/binary allocation size.
    size: u32,
    /// Numeric or temporal scale where the selected type supports one.
    scale: u8,
}

impl ParameterHint {
    /// Validates the type code and numeric bounds once so conversion matches
    /// can treat any remaining arm as unreachable.
    pub(crate) fn new(sql_type: i32, size: u32, scale: u8) -> TdsResult<Self> {
        let sql_type = InputSqlType::try_from(sql_type)?;
        if matches!(sql_type, InputSqlType::Numeric | InputSqlType::Decimal) {
            let precision = Self::effective_numeric_precision(size);
            if precision > 38 || u32::from(scale) > precision {
                return Err(Error::UsageError(format!(
                    "Invalid numeric precision/scale: precision={precision}, scale={scale}"
                )));
            }
        }
        if matches!(
            sql_type,
            InputSqlType::Time | InputSqlType::DateTime | InputSqlType::DateTimeOffset
        ) && scale > 7
        {
            return Err(Error::UsageError(format!(
                "Invalid temporal scale: {scale}. Expected a value from 0 to 7"
            )));
        }
        Ok(Self {
            sql_type,
            size,
            scale,
        })
    }

    fn effective_numeric_precision(size: u32) -> u32 {
        if size == 0 { 18 } else { size }
    }

    /// Whether SQL Server permits this type in TVP column metadata.
    pub(crate) fn supports_tvp_column(self) -> bool {
        !matches!(
            self.sql_type,
            InputSqlType::Udt | InputSqlType::LongVarChar | InputSqlType::WLongVarChar
        )
    }

    /// Returns effective decimal precision for TVP metadata.
    pub(crate) fn precision(self) -> Option<u8> {
        matches!(self.sql_type, InputSqlType::Numeric | InputSqlType::Decimal)
            .then(|| Self::effective_numeric_precision(self.size) as u8)
    }

    /// Returns scale only for numeric and scale-bearing temporal types.
    pub(crate) fn scale(self) -> Option<u8> {
        matches!(
            self.sql_type,
            InputSqlType::Numeric
                | InputSqlType::Decimal
                | InputSqlType::Time
                | InputSqlType::DateTime
                | InputSqlType::DateTimeOffset
        )
        .then_some(self.scale)
    }
}

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

/// Convert a Python value to the inferred SQL type used by RPC parameters.
pub(crate) fn py_to_sql_type(py_obj: &Bound<'_, PyAny>) -> TdsResult<SqlType> {
    if py_obj.is_none() {
        return Ok(SqlType::NVarchar(None, 1));
    }

    if py_obj.is_instance_of::<PyBool>() {
        return py_obj
            .extract::<bool>()
            .map(|value| SqlType::Bit(Some(value)))
            .map_err(|error| Error::UsageError(format!("Failed to extract bool: {error}")));
    }

    if py_obj.is_instance_of::<PyInt>() {
        let value = py_obj.extract::<i64>().map_err(|error| {
            Error::UsageError(format!(
                "Integer parameter is outside the BIGINT range: {error}"
            ))
        })?;
        return Ok(match value {
            0..=255 => SqlType::TinyInt(Some(value as u8)),
            -32_768..=32_767 => SqlType::SmallInt(Some(value as i16)),
            -2_147_483_648..=2_147_483_647 => SqlType::Int(Some(value as i32)),
            _ => SqlType::BigInt(Some(value)),
        });
    }

    if py_obj.is_exact_instance_of::<PyFloat>() {
        return py_obj
            .extract::<f64>()
            .map(|value| SqlType::Float(Some(value)))
            .map_err(|error| Error::UsageError(format!("Failed to extract float: {error}")));
    }

    if py_obj.is_instance_of::<PyString>() {
        let value = py_obj
            .extract::<String>()
            .map_err(|error| Error::UsageError(format!("Failed to extract string: {error}")))?;
        let is_ascii = value.is_ascii();
        let length = if is_ascii {
            value.len()
        } else {
            value.encode_utf16().count()
        }
        .max(1);
        let value = SqlString::from_utf8_string(value);
        return Ok(if is_ascii {
            match u16::try_from(length) {
                Ok(length) if length <= 8_000 => SqlType::Varchar(Some(value), length),
                _ => SqlType::VarcharMax(Some(value)),
            }
        } else {
            match u16::try_from(length) {
                Ok(length) if length <= 4_000 => SqlType::NVarchar(Some(value), length),
                _ => SqlType::NVarcharMax(Some(value)),
            }
        });
    }

    if py_obj.is_instance_of::<PyDateTime>() {
        return py_datetime_to_sql_type(py_obj, None);
    }

    if py_obj.is_instance_of::<PyTime>() {
        let time = py_time(py_obj, 6)?;
        return Ok(SqlType::Time(Some(time)));
    }

    if is_decimal(py_obj)? {
        return py_decimal_to_sql_type(py_obj);
    }

    let value = py_to_column_value(py_obj, None)?;
    Ok(match value {
        ColumnValues::TinyInt(value) => SqlType::TinyInt(Some(value)),
        ColumnValues::SmallInt(value) => SqlType::SmallInt(Some(value)),
        ColumnValues::Int(value) => SqlType::Int(Some(value)),
        ColumnValues::BigInt(value) => SqlType::BigInt(Some(value)),
        ColumnValues::Real(value) => SqlType::Real(Some(value)),
        ColumnValues::Float(value) => SqlType::Float(Some(value)),
        ColumnValues::Decimal(value) => SqlType::Decimal(Some(value)),
        ColumnValues::Numeric(value) => SqlType::Numeric(Some(value)),
        ColumnValues::Bit(value) => SqlType::Bit(Some(value)),
        ColumnValues::String(value) => {
            let length = value.bytes.len().div_ceil(2).max(1);
            match u16::try_from(length) {
                Ok(length) if length <= 4_000 => SqlType::NVarchar(Some(value), length),
                _ => SqlType::NVarcharMax(Some(value)),
            }
        }
        ColumnValues::DateTime(value) => SqlType::DateTime(Some(value)),
        ColumnValues::Date(value) => SqlType::Date(Some(value)),
        ColumnValues::Time(value) => SqlType::Time(Some(value)),
        ColumnValues::DateTime2(value) => SqlType::DateTime2(Some(value)),
        ColumnValues::DateTimeOffset(value) => SqlType::DateTimeOffset(Some(value)),
        ColumnValues::SmallDateTime(value) => SqlType::SmallDateTime(Some(value)),
        ColumnValues::SmallMoney(value) => SqlType::SmallMoney(Some(value)),
        ColumnValues::Money(value) => SqlType::Money(Some(value)),
        ColumnValues::Bytes(value) => match u16::try_from(value.len().max(1)) {
            Ok(length) if length <= 8_000 => SqlType::VarBinary(Some(value), length),
            _ => SqlType::VarBinaryMax(Some(value)),
        },
        ColumnValues::Xml(value) => SqlType::Xml(Some(value)),
        ColumnValues::Null => SqlType::NVarchar(None, 1),
        ColumnValues::Uuid(value) => SqlType::Uuid(Some(value)),
        ColumnValues::Json(value) => SqlType::Json(Some(value)),
        ColumnValues::Vector(value) => {
            let dimensions = value.dimension_count();
            let base_type = value.base_type();
            SqlType::Vector(Some(value), dimensions, base_type)
        }
    })
}

/// Converts a Python value according to its validated `setinputsizes()` hint.
///
/// Hints select the SQL type, but size handling remains type-specific: decimal
/// precision and scale are authoritative, strings and binary values widen to
/// fit their contents, and vector dimensions are validated against the value.
pub(crate) fn py_to_sql_type_with_hint(
    py_obj: &Bound<'_, PyAny>,
    hint: ParameterHint,
) -> TdsResult<SqlType> {
    if py_obj.is_none() {
        return null_sql_type(hint);
    }

    match hint.sql_type {
        InputSqlType::Bit => Ok(SqlType::Bit(Some(py_obj.extract::<bool>().map_err(|error| {
            Error::UsageError(format!("Failed to convert parameter to BIT: {error}"))
        })?))),
        InputSqlType::TinyInt => Ok(SqlType::TinyInt(Some(py_obj.extract::<u8>().map_err(
            |error| Error::UsageError(format!("Failed to convert parameter to TINYINT: {error}")),
        )?))),
        InputSqlType::SmallInt => Ok(SqlType::SmallInt(Some(py_obj.extract::<i16>().map_err(
            |error| Error::UsageError(format!("Failed to convert parameter to SMALLINT: {error}")),
        )?))),
        InputSqlType::Integer => Ok(SqlType::Int(Some(py_obj.extract::<i32>().map_err(
            |error| Error::UsageError(format!("Failed to convert parameter to INT: {error}")),
        )?))),
        InputSqlType::BigInt => Ok(SqlType::BigInt(Some(py_obj.extract::<i64>().map_err(
            |error| Error::UsageError(format!("Failed to convert parameter to BIGINT: {error}")),
        )?))),
        InputSqlType::Real => Ok(SqlType::Real(Some(py_obj.extract::<f32>().map_err(
            |error| Error::UsageError(format!("Failed to convert parameter to REAL: {error}")),
        )?))),
        InputSqlType::Float => Ok(SqlType::Float(Some(py_obj.extract::<f64>().map_err(
            |error| Error::UsageError(format!("Failed to convert parameter to FLOAT: {error}")),
        )?))),
        InputSqlType::Numeric | InputSqlType::Decimal => hinted_decimal(py_obj, hint),
        InputSqlType::Char
        | InputSqlType::VarChar
        | InputSqlType::LongVarChar
        | InputSqlType::WChar
        | InputSqlType::WVarChar
        | InputSqlType::WLongVarChar => hinted_string(py_obj, hint),
        InputSqlType::Binary | InputSqlType::VarBinary | InputSqlType::LongVarBinary => {
            hinted_binary(py_obj, hint)
        }
        InputSqlType::Date => match py_to_sql_type(py_obj)? {
            SqlType::Date(value) => Ok(SqlType::Date(value)),
            _ => Err(Error::UsageError("Expected a date parameter".to_string())),
        },
        InputSqlType::Time => Ok(SqlType::Time(Some(py_time(py_obj, hint.scale)?))),
        InputSqlType::DateTime => match py_datetime_to_sql_type(py_obj, Some(hint.scale))? {
            SqlType::DateTime2(value) => Ok(SqlType::DateTime2(value)),
            SqlType::DateTimeOffset(value) => Ok(SqlType::DateTime2(
                value.map(|value| value.datetime2),
            )),
            _ => unreachable!("datetime conversion returns a temporal SQL type"),
        },
        InputSqlType::DateTimeOffset => match py_datetime_to_sql_type(py_obj, Some(hint.scale))? {
            SqlType::DateTimeOffset(value) => Ok(SqlType::DateTimeOffset(value)),
            _ => Err(Error::UsageError(
                "DATETIMEOFFSET requires a timezone-aware datetime".to_string(),
            )),
        },
        InputSqlType::Guid => match py_to_sql_type(py_obj)? {
            SqlType::Uuid(value) => Ok(SqlType::Uuid(value)),
            _ => Err(Error::UsageError("Expected a UUID parameter".to_string())),
        },
        InputSqlType::Xml => {
            let value = py_obj.extract::<String>().map_err(|error| {
                Error::UsageError(format!("Failed to convert parameter to XML: {error}"))
            })?;
            Ok(SqlType::Xml(Some(SqlXml::from(value))))
        }
        InputSqlType::Json => hinted_json(py_obj),
        InputSqlType::Vector => hinted_vector(py_obj, hint),
        InputSqlType::Money | InputSqlType::SmallMoney => hinted_money(py_obj, hint),
        InputSqlType::Variant => Ok(SqlType::Variant(Box::new(py_to_sql_type(py_obj)?))),
        // TODO(mssql-tds): Add a public SqlType::Udt input contract and RPC
        // serializer carrying database, schema, and server UDT type names.
        InputSqlType::Udt => Err(Error::UsageError(
            "SQL_SS_UDT parameters require a server UDT type name, which setinputsizes does not provide"
                .to_string(),
        )),
    }
}

/// Constructs the typed NULL required when inference has no Python value.
///
/// Variable-length types may promote to `MAX`; fixed-width types reject sizes
/// beyond SQL Server limits. NULL numeric and temporal hints are accepted only
/// when their metadata matches the value-free [`SqlType`] defaults.
pub(crate) fn null_sql_type(hint: ParameterHint) -> TdsResult<SqlType> {
    let size = hint.size.max(1);
    Ok(match hint.sql_type {
        InputSqlType::Bit => SqlType::Bit(None),
        InputSqlType::TinyInt => SqlType::TinyInt(None),
        InputSqlType::SmallInt => SqlType::SmallInt(None),
        InputSqlType::Integer => SqlType::Int(None),
        InputSqlType::BigInt => SqlType::BigInt(None),
        InputSqlType::Real => SqlType::Real(None),
        InputSqlType::Float => SqlType::Float(None),
        InputSqlType::Numeric | InputSqlType::Decimal => {
            // TODO(mssql-tds): Carry NULL precision and scale independently of
            // the optional value so py-core can preserve non-default metadata.
            if hint.size != 18 || hint.scale != 10 {
                return Err(Error::UsageError(format!(
                    "NULL numeric parameters currently require precision 18 and scale 10; requested precision {} and scale {}",
                    hint.size, hint.scale
                )));
            }
            if hint.sql_type == InputSqlType::Numeric {
                SqlType::Numeric(None)
            } else {
                SqlType::Decimal(None)
            }
        }
        InputSqlType::Char => SqlType::Char(None, checked_length(size, 8_000, "CHAR")?),
        InputSqlType::VarChar => sized_varchar(None, size),
        InputSqlType::LongVarChar => SqlType::VarcharMax(None),
        InputSqlType::WChar => SqlType::NChar(None, checked_length(size, 4_000, "NCHAR")?),
        InputSqlType::WVarChar => sized_nvarchar(None, size),
        InputSqlType::WLongVarChar => SqlType::NText(None),
        InputSqlType::Binary => SqlType::Binary(None, checked_length(size, 8_000, "BINARY")?),
        InputSqlType::VarBinary => sized_varbinary(None, size),
        InputSqlType::LongVarBinary | InputSqlType::Udt => SqlType::VarBinaryMax(None),
        InputSqlType::Date => SqlType::Date(None),
        InputSqlType::Time | InputSqlType::DateTime | InputSqlType::DateTimeOffset => {
            // TODO(mssql-tds): Carry NULL temporal scale independently of the
            // optional value so py-core can preserve non-default metadata.
            if hint.scale != 7 {
                return Err(Error::UsageError(format!(
                    "NULL temporal parameters currently require scale 7; requested scale {}",
                    hint.scale
                )));
            }
            match hint.sql_type {
                InputSqlType::Time => SqlType::Time(None),
                InputSqlType::DateTime => SqlType::DateTime2(None),
                InputSqlType::DateTimeOffset => SqlType::DateTimeOffset(None),
                _ => unreachable!("matched temporal input type"),
            }
        }
        InputSqlType::Guid => SqlType::Uuid(None),
        InputSqlType::Xml => SqlType::Xml(None),
        InputSqlType::Json => SqlType::Json(None),
        InputSqlType::Vector => {
            if hint.size == 0 {
                return Err(Error::UsageError(
                    "A NULL VECTOR parameter requires its dimension count as the input size"
                        .to_string(),
                ));
            }
            SqlType::Vector(
                None,
                checked_length(hint.size, 1_998, "VECTOR")?,
                mssql_tds::datatypes::sqldatatypes::VectorBaseType::Float32,
            )
        }
        InputSqlType::Money => SqlType::Money(None),
        InputSqlType::SmallMoney => SqlType::SmallMoney(None),
        InputSqlType::Variant => SqlType::Variant(Box::new(SqlType::NVarchar(None, 1))),
    })
}

/// Applies caller-specified decimal precision and scale instead of inferring
/// them from the Python value.
fn hinted_decimal(py_obj: &Bound<'_, PyAny>, hint: ParameterHint) -> TdsResult<SqlType> {
    let precision = ParameterHint::effective_numeric_precision(hint.size) as u8;
    let value = py_obj
        .str()
        .and_then(|value| value.extract::<String>())
        .map_err(|error| Error::UsageError(format!("Failed to convert numeric value: {error}")))?;
    let parts = DecimalParts::from_string(&value, precision, hint.scale)?;
    Ok(if hint.sql_type == InputSqlType::Numeric {
        SqlType::Numeric(Some(parts))
    } else {
        SqlType::Decimal(Some(parts))
    })
}

/// Preserves the requested string allocation while widening it when needed to
/// avoid declaring a type shorter than the encoded value.
fn hinted_string(py_obj: &Bound<'_, PyAny>, hint: ParameterHint) -> TdsResult<SqlType> {
    let value = py_obj.extract::<String>().map_err(|error| {
        Error::UsageError(format!("Failed to convert parameter to string: {error}"))
    })?;
    let actual = if matches!(
        hint.sql_type,
        InputSqlType::WChar | InputSqlType::WVarChar | InputSqlType::WLongVarChar
    ) {
        value.encode_utf16().count() as u32
    } else {
        value.len() as u32
    }
    .max(1);
    let size = hint.size.max(actual);
    let value = Some(SqlString::from_utf8_string(value));
    Ok(match hint.sql_type {
        InputSqlType::Char => SqlType::Char(value, checked_length(size, 8_000, "CHAR")?),
        InputSqlType::VarChar => sized_varchar(value, size),
        InputSqlType::LongVarChar => SqlType::VarcharMax(value),
        InputSqlType::WChar => SqlType::NChar(value, checked_length(size, 4_000, "NCHAR")?),
        InputSqlType::WVarChar => sized_nvarchar(value, size),
        InputSqlType::WLongVarChar => SqlType::NText(value),
        _ => unreachable!("hinted_string receives only string constants"),
    })
}

/// Preserves the requested binary allocation while widening it to fit the value.
fn hinted_binary(py_obj: &Bound<'_, PyAny>, hint: ParameterHint) -> TdsResult<SqlType> {
    let value = py_obj.extract::<Vec<u8>>().map_err(|error| {
        Error::UsageError(format!("Failed to convert parameter to binary: {error}"))
    })?;
    let size = hint.size.max(value.len() as u32).max(1);
    Ok(match hint.sql_type {
        InputSqlType::Binary => {
            SqlType::Binary(Some(value), checked_length(size, 8_000, "BINARY")?)
        }
        InputSqlType::VarBinary => sized_varbinary(Some(value), size),
        InputSqlType::LongVarBinary => SqlType::VarBinaryMax(Some(value)),
        _ => unreachable!("hinted_binary receives only binary constants"),
    })
}

/// Serializes arbitrary Python values through `json.dumps` for SQL JSON parameters.
fn hinted_json(py_obj: &Bound<'_, PyAny>) -> TdsResult<SqlType> {
    let py = py_obj.py();
    let dumps = JSON_DUMPS
        .get_or_try_init(py, || {
            PyModule::import(py, "json")
                .and_then(|module| module.getattr("dumps"))
                .map(Bound::unbind)
        })
        .map(|dumps| dumps.bind(py));
    let value = dumps
        .and_then(|dumps| dumps.call1((py_obj,)))
        .and_then(|value| value.extract::<String>())
        .map_err(|error| {
            Error::UsageError(format!("Failed to serialize JSON parameter: {error}"))
        })?;
    Ok(SqlType::Json(Some(SqlJson::from(value))))
}

/// Converts a float32 vector and rejects a conflicting hinted dimension count.
fn hinted_vector(py_obj: &Bound<'_, PyAny>, hint: ParameterHint) -> TdsResult<SqlType> {
    let values = py_obj.extract::<Vec<f32>>().map_err(|error| {
        Error::UsageError(format!("Failed to convert parameter to VECTOR: {error}"))
    })?;
    let vector = SqlVector::try_from_f32(values)?;
    let dimensions = vector.dimension_count();
    if hint.size != 0 && hint.size != u32::from(dimensions) {
        return Err(Error::UsageError(format!(
            "VECTOR input size {} does not match value dimension count {dimensions}",
            hint.size
        )));
    }
    Ok(SqlType::Vector(
        Some(vector),
        dimensions,
        mssql_tds::datatypes::sqldatatypes::VectorBaseType::Float32,
    ))
}

/// Quantizes through Python `Decimal` so MONEY values use SQL Server's fixed
/// four-digit scale before range-checking SMALLMONEY.
fn hinted_money(py_obj: &Bound<'_, PyAny>, hint: ParameterHint) -> TdsResult<SqlType> {
    let decimal_class = decimal_type(py_obj.py())
        .map_err(|error| Error::UsageError(format!("Failed to import Decimal: {error}")))?;
    let value = py_obj
        .str()
        .map_err(|error| Error::UsageError(format!("Failed to read money value: {error}")))?;
    let decimal = decimal_class
        .call1((value,))
        .map_err(|error| Error::UsageError(format!("Failed to convert money value: {error}")))?;
    let quantizer = decimal_class
        .call1(("0.0001",))
        .map_err(|error| Error::UsageError(format!("Failed to create money scale: {error}")))?;
    let scaled = decimal
        .call_method1("quantize", (quantizer,))
        .and_then(|value| value.call_method1("scaleb", (4,)))
        .and_then(|value| value.call_method0("__int__"))
        .and_then(|value| value.extract::<i64>())
        .map_err(|error| Error::UsageError(format!("Failed to scale money value: {error}")))?;

    if hint.sql_type == InputSqlType::SmallMoney {
        let scaled = i32::try_from(scaled)
            .map_err(|_| Error::UsageError("Value is outside the SMALLMONEY range".to_string()))?;
        Ok(SqlType::SmallMoney(Some(SqlSmallMoney { int_val: scaled })))
    } else {
        Ok(SqlType::Money(Some(SqlMoney {
            lsb_part: scaled as u32 as i32,
            msb_part: (scaled >> 32) as i32,
        })))
    }
}

/// Enforces fixed-width SQL Server limits before narrowing a size to `u16`.
fn checked_length(size: u32, maximum: u32, type_name: &str) -> TdsResult<u16> {
    if size > maximum {
        return Err(Error::UsageError(format!(
            "{type_name} size {size} exceeds the maximum {maximum}"
        )));
    }
    Ok(size as u16)
}

/// Selects `varchar(n)` when representable and `varchar(max)` otherwise.
fn sized_varchar(value: Option<SqlString>, size: u32) -> SqlType {
    match u16::try_from(size) {
        Ok(size) if size <= 8_000 => SqlType::Varchar(value, size),
        _ => SqlType::VarcharMax(value),
    }
}

/// Selects `nvarchar(n)` when representable and `nvarchar(max)` otherwise.
fn sized_nvarchar(value: Option<SqlString>, size: u32) -> SqlType {
    match u16::try_from(size) {
        Ok(size) if size <= 4_000 => SqlType::NVarchar(value, size),
        _ => SqlType::NVarcharMax(value),
    }
}

/// Selects `varbinary(n)` when representable and `varbinary(max)` otherwise.
fn sized_varbinary(value: Option<Vec<u8>>, size: u32) -> SqlType {
    match u16::try_from(size) {
        Ok(size) if size <= 8_000 => SqlType::VarBinary(value, size),
        _ => SqlType::VarBinaryMax(value),
    }
}

// TODO(performance): Cache or compute datetime base ordinals in the bulk-copy
// conversion path instead of calling Python's toordinal for each value.
fn py_datetime_to_sql_type(
    py_obj: &Bound<'_, PyAny>,
    hinted_scale: Option<u8>,
) -> TdsResult<SqlType> {
    let ordinal = py_obj
        .call_method0("toordinal")
        .and_then(|value| value.extract::<u32>())
        .map_err(|error| Error::UsageError(format!("Failed to get datetime ordinal: {error}")))?;
    let days = ordinal.checked_sub(1).ok_or_else(|| {
        Error::UsageError("Date ordinal is 0, expected a value greater than 0".to_string())
    })?;

    let tzinfo = py_obj
        .getattr("tzinfo")
        .map_err(|error| Error::UsageError(format!("Failed to get datetime tzinfo: {error}")))?;
    if tzinfo.is_none() {
        return Ok(SqlType::DateTime2(Some(SqlDateTime2 {
            days,
            time: py_time(py_obj, hinted_scale.unwrap_or(6))?,
        })));
    }

    let offset = py_obj
        .call_method0("utcoffset")
        .map_err(|error| Error::UsageError(format!("Failed to get timezone offset: {error}")))?;
    if offset.is_none() {
        return Err(Error::UsageError(
            "Timezone-aware datetime returned no UTC offset".to_string(),
        ));
    }
    let offset_seconds = offset
        .call_method0("total_seconds")
        .and_then(|value| value.extract::<f64>())
        .map_err(|error| {
            Error::UsageError(format!("Failed to get timezone offset seconds: {error}"))
        })?;
    let offset_minutes = datetimeoffset_minutes(offset_seconds)?;

    Ok(SqlType::DateTimeOffset(Some(SqlDateTimeOffset {
        datetime2: SqlDateTime2 {
            days,
            time: py_time(py_obj, hinted_scale.unwrap_or(7))?,
        },
        offset: offset_minutes,
    })))
}

fn datetimeoffset_minutes(offset_seconds: f64) -> TdsResult<i16> {
    if !offset_seconds.is_finite() || offset_seconds % 60.0 != 0.0 {
        return Err(Error::UsageError(
            "DATETIMEOFFSET requires a whole-minute UTC offset".to_string(),
        ));
    }
    let offset_minutes = offset_seconds / 60.0;
    if !(-840.0..=840.0).contains(&offset_minutes) {
        return Err(Error::UsageError(format!(
            "Timezone offset {offset_minutes} minutes is outside the DATETIMEOFFSET range"
        )));
    }
    Ok(offset_minutes as i16)
}

/// Converts Python time components to the 100-nanosecond units used by TDS,
/// retaining the scale selected by inference or the caller's hint.
fn py_time(py_obj: &Bound<'_, PyAny>, scale: u8) -> TdsResult<SqlTime> {
    let component = |name: &str| {
        py_obj
            .getattr(name)
            .and_then(|value| value.extract::<u64>())
            .map_err(|error| Error::UsageError(format!("Failed to get {name}: {error}")))
    };
    let time_nanoseconds = component("hour")? * 36_000_000_000
        + component("minute")? * 600_000_000
        + component("second")? * 10_000_000
        + component("microsecond")? * 10;
    Ok(SqlTime {
        time_nanoseconds,
        scale,
    })
}

/// Uses Python's `Decimal` class identity rather than accepting lookalike values.
fn is_decimal(py_obj: &Bound<'_, PyAny>) -> TdsResult<bool> {
    let decimal = decimal_type(py_obj.py())
        .map_err(|error| Error::UsageError(format!("Failed to import decimal.Decimal: {error}")))?;
    py_obj
        .is_instance(decimal)
        .map_err(|error| Error::UsageError(format!("Failed to inspect Decimal value: {error}")))
}

fn inferred_decimal_shape(digit_count: usize, exponent: i64) -> TdsResult<(u8, u8)> {
    let precision = if exponent >= 0 {
        digit_count.saturating_add(exponent as usize)
    } else {
        digit_count.max(exponent.unsigned_abs() as usize)
    };
    if precision == 0 || precision > 38 {
        return Err(Error::UsageError(format!(
            "Decimal precision {precision} is outside SQL Server's supported range of 1 to 38"
        )));
    }
    let scale = if exponent < 0 {
        exponent.unsigned_abs() as usize
    } else {
        0
    };
    if scale > 38 {
        return Err(Error::UsageError(format!(
            "Decimal scale {scale} exceeds SQL Server's maximum scale of 38"
        )));
    }
    Ok((precision as u8, scale as u8))
}

fn py_decimal_to_sql_type(py_obj: &Bound<'_, PyAny>) -> TdsResult<SqlType> {
    let decimal_tuple = py_obj
        .call_method0("as_tuple")
        .map_err(|error| Error::UsageError(format!("Failed to inspect Decimal value: {error}")))?;
    let digit_count = decimal_tuple
        .getattr("digits")
        .and_then(|digits| digits.len())
        .map_err(|error| Error::UsageError(format!("Failed to inspect Decimal digits: {error}")))?;
    let exponent = decimal_tuple
        .getattr("exponent")
        .and_then(|value| value.extract::<i64>())
        .map_err(|_| {
            Error::UsageError("NaN and infinite Decimal values are unsupported".to_string())
        })?;
    let (precision, scale) = inferred_decimal_shape(digit_count, exponent)?;
    let value = py_obj
        .call_method0("__str__")
        .and_then(|value| value.extract::<String>())
        .map_err(|error| Error::UsageError(format!("Failed to extract Decimal value: {error}")))?;
    let value = DecimalParts::from_string(&value, precision, scale)?;
    Ok(SqlType::Numeric(Some(value)))
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
                        datetimeoffset_minutes(offset_seconds)?
                    }
                    _ => {
                        // No timezone info, default to UTC (0 offset)
                        0
                    }
                };

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
    if let Ok(decimal_class) = decimal_type(py)
        && let Ok(is_instance) = py_obj.is_instance(decimal_class)
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
    if let Ok(uuid_class) = uuid_type(py)
        && let Ok(is_instance) = py_obj.is_instance(uuid_class)
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

        // Binary (including legacy IMAGE type). UDT is included because a CLR
        // UDT is bulk-copied as its varbinary(max) serialized form (GH-667), so
        // a Python bytes/bytearray value binds to a UDT column as Bytes.
        (
            ColumnValues::Bytes(_),
            SqlDbType::VarBinary | SqlDbType::Binary | SqlDbType::Image | SqlDbType::Udt,
        ) => true,

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
    fn numeric_hints_validate_effective_precision() {
        for sql_type in [2, 3] {
            assert!(ParameterHint::new(sql_type, 0, 0).is_ok());
            assert!(ParameterHint::new(sql_type, 0, 2).is_ok());
            assert!(ParameterHint::new(sql_type, 2, 3).is_err());
            assert!(ParameterHint::new(sql_type, 39, 0).is_err());
            assert!(ParameterHint::new(sql_type, 38, 39).is_err());
            assert_eq!(
                ParameterHint::new(sql_type, 0, 2).unwrap().precision(),
                Some(18)
            );
        }
    }

    #[test]
    fn temporal_hints_accept_scales_zero_through_seven() {
        for sql_type in [10, 11, 92, 93, -154, -155] {
            for scale in [0, 3, 7] {
                assert!(ParameterHint::new(sql_type, 0, scale).is_ok());
            }
            assert!(ParameterHint::new(sql_type, 0, 8).is_err());
        }
    }

    #[test]
    fn null_numeric_accepts_only_core_default_metadata() {
        for sql_type in [2, 3] {
            assert!(null_sql_type(ParameterHint::new(sql_type, 18, 10).unwrap()).is_ok());
            assert!(null_sql_type(ParameterHint::new(sql_type, 9, 2).unwrap()).is_err());
            assert!(null_sql_type(ParameterHint::new(sql_type, 0, 0).unwrap()).is_err());
        }
    }

    #[test]
    fn null_temporal_accepts_only_core_default_scale() {
        for sql_type in [92, 93, -155] {
            assert!(null_sql_type(ParameterHint::new(sql_type, 0, 7).unwrap()).is_ok());
            assert!(null_sql_type(ParameterHint::new(sql_type, 0, 3).unwrap()).is_err());
            assert!(null_sql_type(ParameterHint::new(sql_type, 0, 0).unwrap()).is_err());
        }
    }

    #[test]
    fn datetimeoffset_requires_whole_minute_offsets() {
        assert!(datetimeoffset_minutes(30.0).is_err());
        assert!(datetimeoffset_minutes(-30.0).is_err());
        assert!(datetimeoffset_minutes(f64::NAN).is_err());
        assert!(datetimeoffset_minutes(f64::INFINITY).is_err());
    }

    #[test]
    fn datetimeoffset_accepts_range_boundaries() {
        assert_eq!(datetimeoffset_minutes(14.0 * 60.0 * 60.0).unwrap(), 840);
        assert_eq!(datetimeoffset_minutes(-14.0 * 60.0 * 60.0).unwrap(), -840);
        assert!(datetimeoffset_minutes(841.0 * 60.0).is_err());
        assert!(datetimeoffset_minutes(-841.0 * 60.0).is_err());
    }

    #[test]
    fn decimal_shape_handles_positive_and_negative_exponents() {
        assert_eq!(inferred_decimal_shape(1, 3).unwrap(), (4, 0));
        assert_eq!(inferred_decimal_shape(1, -3).unwrap(), (3, 3));
    }

    #[test]
    fn decimal_shape_enforces_precision_38_boundaries() {
        assert_eq!(inferred_decimal_shape(38, 0).unwrap(), (38, 0));
        assert!(inferred_decimal_shape(39, 0).is_err());
        assert_eq!(inferred_decimal_shape(1, 37).unwrap(), (38, 0));
        assert!(inferred_decimal_shape(1, 38).is_err());
        assert_eq!(inferred_decimal_shape(1, -38).unwrap(), (38, 38));
        assert!(inferred_decimal_shape(1, -39).is_err());
    }

    #[test]
    fn input_sql_type_accepts_every_supported_odbc_code() {
        let supported = [
            -155, -154, -152, -151, -150, -11, -10, -9, -8, -7, -6, -5, -4, -3, -2, -1, 1, 2, 3, 4,
            5, 6, 7, 8, 9, 10, 11, 12, 60, 91, 92, 93, 122, 241, 244, 245,
        ];

        for code in supported {
            assert!(InputSqlType::try_from(code).is_ok(), "ODBC code {code}");
        }
    }

    #[test]
    fn input_sql_type_normalizes_equivalent_odbc_aliases() {
        assert_eq!(InputSqlType::try_from(6).unwrap(), InputSqlType::Float);
        assert_eq!(InputSqlType::try_from(8).unwrap(), InputSqlType::Float);
        assert_eq!(InputSqlType::try_from(9).unwrap(), InputSqlType::Date);
        assert_eq!(InputSqlType::try_from(91).unwrap(), InputSqlType::Date);
        assert_eq!(InputSqlType::try_from(10).unwrap(), InputSqlType::Time);
        assert_eq!(InputSqlType::try_from(92).unwrap(), InputSqlType::Time);
        assert_eq!(InputSqlType::try_from(-154).unwrap(), InputSqlType::Time);
        assert_eq!(InputSqlType::try_from(11).unwrap(), InputSqlType::DateTime);
        assert_eq!(InputSqlType::try_from(93).unwrap(), InputSqlType::DateTime);
        assert_eq!(InputSqlType::try_from(-152).unwrap(), InputSqlType::Xml);
        assert_eq!(InputSqlType::try_from(241).unwrap(), InputSqlType::Xml);
    }

    #[test]
    fn input_sql_type_rejects_unknown_odbc_code() {
        let error = InputSqlType::try_from(0).unwrap_err();
        assert!(error.to_string().contains("Invalid SQL type: 0"));
    }

    #[test]
    fn parameter_hint_reports_tvp_column_support_symbolically() {
        for code in [-151, -1, -10] {
            assert!(
                !ParameterHint::new(code, 0, 0)
                    .unwrap()
                    .supports_tvp_column()
            );
        }
        assert!(ParameterHint::new(-9, 20, 0).unwrap().supports_tvp_column());
    }

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

    #[test]
    fn validate_bytes_accepted_for_udt_column() {
        // GH-667: a CLR UDT column is bulk-copied as its varbinary(max)
        // serialized form, so a Python bytes value (ColumnValues::Bytes) must
        // validate against a UDT target, not only VarBinary/Binary/Image.
        let udt = BulkCopyColumnMetadata::new("g", SqlDbType::Udt, 0xA5);
        let bytes = ColumnValues::Bytes(vec![0xDE, 0xAD, 0xBE, 0xEF]);
        assert!(validate_type_compatibility(&bytes, &udt).is_ok());

        // Still accepted for varbinary, and NULL remains valid for a UDT column.
        let varbinary = BulkCopyColumnMetadata::new("b", SqlDbType::VarBinary, 0xA5);
        assert!(validate_type_compatibility(&bytes, &varbinary).is_ok());
        assert!(validate_type_compatibility(&ColumnValues::Null, &udt).is_ok());

        // A non-bytes value into a UDT column is still a mismatch.
        assert!(validate_type_compatibility(&ColumnValues::Int(1), &udt).is_err());
    }
}
