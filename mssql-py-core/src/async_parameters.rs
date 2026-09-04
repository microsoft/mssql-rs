// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use mssql_tds::datatypes::sql_tvp::{TvpColumnDef, TvpTableData, TvpTypeName};
use mssql_tds::datatypes::sqldatatypes::VectorBaseType;
use mssql_tds::datatypes::sqltypes::SqlType;
use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};
use pyo3::exceptions::{PyKeyError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyTuple};

use crate::types::{ParameterHint, null_sql_type, py_to_sql_type, py_to_sql_type_with_hint};

/// Comparable SQL declaration metadata used to decide whether a prepared
/// statement can be reused with a new set of bound values.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ParameterMetadata {
    /// A type whose declaration is fully represented by its SQL type name.
    Scalar(&'static str),
    /// A decimal declaration with its family, precision, and scale.
    Decimal {
        /// Whether the declaration uses `numeric` rather than `decimal`.
        numeric: bool,
        /// Declared precision.
        precision: u8,
        /// Declared scale.
        scale: u8,
    },
    /// A temporal declaration whose identity includes fractional-second scale.
    Temporal {
        /// SQL temporal type name.
        kind: &'static str,
        /// Declared fractional-second scale.
        scale: u8,
    },
    /// A character or binary declaration whose identity includes length.
    Sized {
        /// SQL character or binary type name.
        kind: &'static str,
        /// Declared type length.
        length: u16,
    },
    /// A vector declaration identified by dimension count and element type.
    Vector {
        /// Number of vector dimensions.
        dimensions: u16,
        /// SQL vector element type.
        base_type: VectorBaseType,
    },
    /// A `sql_variant` declaration carrying the metadata of its value.
    Variant(Box<ParameterMetadata>),
    /// A table-valued parameter declaration identified by server type name.
    Table {
        /// SQL schema containing the table type.
        schema: String,
        /// Unqualified table type name.
        name: String,
    },
}

/// Extracts the declaration identity used for prepared-statement reuse.
fn sql_type_metadata(value: &SqlType) -> ParameterMetadata {
    match value {
        SqlType::Bit(_) => ParameterMetadata::Scalar("bit"),
        SqlType::TinyInt(_) => ParameterMetadata::Scalar("tinyint"),
        SqlType::SmallInt(_) => ParameterMetadata::Scalar("smallint"),
        SqlType::Int(_) => ParameterMetadata::Scalar("int"),
        SqlType::BigInt(_) => ParameterMetadata::Scalar("bigint"),
        SqlType::Real(_) => ParameterMetadata::Scalar("real"),
        SqlType::Float(_) => ParameterMetadata::Scalar("float"),
        SqlType::Decimal(value) => ParameterMetadata::Decimal {
            numeric: false,
            precision: value.as_ref().map_or(18, |value| value.precision),
            scale: value.as_ref().map_or(10, |value| value.scale),
        },
        SqlType::Numeric(value) => ParameterMetadata::Decimal {
            numeric: true,
            precision: value.as_ref().map_or(18, |value| value.precision),
            scale: value.as_ref().map_or(10, |value| value.scale),
        },
        SqlType::Money(_) => ParameterMetadata::Scalar("money"),
        SqlType::SmallMoney(_) => ParameterMetadata::Scalar("smallmoney"),
        SqlType::Time(value) => ParameterMetadata::Temporal {
            kind: "time",
            scale: value.as_ref().map_or(7, |value| value.scale),
        },
        SqlType::DateTime2(value) => ParameterMetadata::Temporal {
            kind: "datetime2",
            scale: value.as_ref().map_or(7, |value| value.time.scale),
        },
        SqlType::DateTimeOffset(value) => ParameterMetadata::Temporal {
            kind: "datetimeoffset",
            scale: value.as_ref().map_or(7, |value| value.datetime2.time.scale),
        },
        SqlType::SmallDateTime(_) => ParameterMetadata::Scalar("smalldatetime"),
        SqlType::DateTime(_) => ParameterMetadata::Scalar("datetime"),
        SqlType::Date(_) => ParameterMetadata::Scalar("date"),
        SqlType::NVarchar(_, length) => ParameterMetadata::Sized {
            kind: "nvarchar",
            length: *length,
        },
        SqlType::NVarcharMax(_) => ParameterMetadata::Scalar("nvarchar(max)"),
        SqlType::Varchar(_, length) => ParameterMetadata::Sized {
            kind: "varchar",
            length: *length,
        },
        SqlType::VarcharMax(_) => ParameterMetadata::Scalar("varchar(max)"),
        SqlType::VarBinary(_, length) => ParameterMetadata::Sized {
            kind: "varbinary",
            length: *length,
        },
        SqlType::VarBinaryMax(_) => ParameterMetadata::Scalar("varbinary(max)"),
        SqlType::Binary(_, length) => ParameterMetadata::Sized {
            kind: "binary",
            length: *length,
        },
        SqlType::Char(_, length) => ParameterMetadata::Sized {
            kind: "char",
            length: *length,
        },
        SqlType::NChar(_, length) => ParameterMetadata::Sized {
            kind: "nchar",
            length: *length,
        },
        SqlType::Text(_) => ParameterMetadata::Scalar("text"),
        SqlType::NText(_) => ParameterMetadata::Scalar("ntext"),
        SqlType::Json(_) => ParameterMetadata::Scalar("json"),
        SqlType::Xml(_) => ParameterMetadata::Scalar("xml"),
        SqlType::Uuid(_) => ParameterMetadata::Scalar("uniqueidentifier"),
        SqlType::Vector(_, dimensions, base_type) => ParameterMetadata::Vector {
            dimensions: *dimensions,
            base_type: *base_type,
        },
        SqlType::Variant(inner) => ParameterMetadata::Variant(Box::new(sql_type_metadata(inner))),
        SqlType::Table(type_name, _) => ParameterMetadata::Table {
            schema: type_name
                .schema_name
                .clone()
                .unwrap_or_else(|| "dbo".to_string()),
            name: type_name.type_name.clone(),
        },
    }
}

/// A SQL Server table-valued parameter for asynchronous execution.
///
/// `type_name` may be `TypeName` or `schema.TypeName`. `columns` contains SQL
/// type hints in `setinputsizes()` format. Omitting `rows` creates a NULL TVP;
/// non-NULL TVPs require both `columns` and `rows`.
#[pyclass(name = "TableValuedParameter", frozen)]
pub(crate) struct PyTableValuedParameter {
    type_name: TvpTypeName,
    table: Option<TvpTableData>,
}

#[pymethods]
impl PyTableValuedParameter {
    /// Create a table-valued parameter.
    #[new]
    #[pyo3(signature = (type_name, columns=None, rows=None, *, schema=None))]
    fn new(
        type_name: String,
        columns: Option<&Bound<'_, PyAny>>,
        rows: Option<&Bound<'_, PyAny>>,
        schema: Option<String>,
    ) -> PyResult<Self> {
        let type_name = parse_tvp_type_name(type_name, schema)?;
        let rows = rows.filter(|rows| !rows.is_none());
        let table = match rows {
            None => None,
            Some(rows) => {
                let columns = columns
                    .filter(|columns| !columns.is_none())
                    .ok_or_else(|| {
                        PyValueError::new_err("A non-NULL TVP requires column definitions")
                    })?;
                Some(build_tvp_table(columns, rows)?)
            }
        };
        Ok(Self { type_name, table })
    }

    /// Unqualified SQL Server table type name.
    #[getter]
    fn type_name(&self) -> &str {
        &self.type_name.type_name
    }

    /// Optional SQL Server schema name.
    #[getter]
    fn schema(&self) -> Option<&str> {
        self.type_name.schema_name.as_deref()
    }

    /// Whether this value represents a NULL TVP.
    #[getter]
    fn is_null(&self) -> bool {
        self.table.is_none()
    }

    /// Number of declared TVP columns, or zero for a NULL TVP.
    #[getter]
    fn column_count(&self) -> usize {
        self.table.as_ref().map_or(0, |table| table.columns.len())
    }

    /// Number of TVP rows, or zero for a NULL TVP.
    #[getter]
    fn row_count(&self) -> usize {
        self.table.as_ref().map_or(0, |table| table.rows.len())
    }
}

impl PyTableValuedParameter {
    /// Converts this Python wrapper into the SQL type consumed by RPC binding.
    fn sql_type(&self) -> SqlType {
        // TODO: Use shared TVP ownership when mssql-tds supports Arc<TvpTableData>.
        SqlType::Table(self.type_name.clone(), self.table.clone())
    }
}

/// Validates and normalizes a possibly schema-qualified TVP type name.
fn parse_tvp_type_name(type_name: String, schema: Option<String>) -> PyResult<TvpTypeName> {
    let parts = type_name.split('.').collect::<Vec<_>>();
    let (schema_name, type_name) = match (schema, parts.as_slice()) {
        (Some(schema), [type_name]) => (Some(schema), *type_name),
        (None, [type_name]) => (None, *type_name),
        (None, [schema, type_name]) => (Some((*schema).to_string()), *type_name),
        (Some(_), _) => {
            return Err(PyValueError::new_err(
                "Specify the TVP schema either in type_name or with schema, not both",
            ));
        }
        (None, _) => {
            return Err(PyValueError::new_err(
                "TVP type_name must be 'TypeName' or 'schema.TypeName'",
            ));
        }
    };
    if type_name.is_empty() || schema_name.as_deref() == Some("") {
        return Err(PyValueError::new_err(
            "TVP schema and type name must not be empty",
        ));
    }
    // TODO(mssql-tds): Escape closing brackets when formatting TVP parameter
    // declarations. Until then, reject names that cannot be represented safely.
    if type_name.contains(']')
        || schema_name
            .as_deref()
            .is_some_and(|schema| schema.contains(']'))
    {
        return Err(PyValueError::new_err(
            "TVP schema and type name must not contain ']'",
        ));
    }
    Ok(TvpTypeName::new(schema_name, type_name.to_string()))
}

/// Builds TVP column metadata and converts each row using its matching hint.
fn build_tvp_table(columns: &Bound<'_, PyAny>, rows: &Bound<'_, PyAny>) -> PyResult<TvpTableData> {
    // TODO(performance): Convert TVP row iterators without first collecting
    // Bound cells, and benchmark representative row counts.
    let hints = parse_input_sizes(columns)?
        .ok_or_else(|| PyValueError::new_err("A TVP must define at least one column"))?;
    let mut column_defs = Vec::with_capacity(hints.len());
    for hint in &hints {
        if !hint.supports_tvp_column() {
            return Err(PyValueError::new_err(
                "TVP columns do not support UDT, TEXT, or NTEXT input types",
            ));
        }
        let mut column = TvpColumnDef::new(
            null_sql_type(*hint).map_err(|error| PyValueError::new_err(error.to_string()))?,
        );
        column.precision = hint.precision();
        column.scale = hint.scale();
        column_defs.push(column);
    }

    let mut converted_rows = Vec::new();
    for (row_index, row) in rows.try_iter()?.enumerate() {
        let row = row?;
        let cells = row.try_iter()?.collect::<PyResult<Vec<_>>>()?;
        if cells.len() != hints.len() {
            return Err(PyValueError::new_err(format!(
                "TVP row {row_index} has {} values but {} columns were declared",
                cells.len(),
                hints.len()
            )));
        }
        let converted = cells
            .iter()
            .zip(&hints)
            .map(|(cell, hint)| {
                py_to_sql_type_with_hint(cell, *hint)
                    .map_err(|error| PyTypeError::new_err(error.to_string()))
            })
            .collect::<PyResult<Vec<_>>>()?;
        converted_rows.push(converted);
    }

    Ok(TvpTableData::new(column_defs, converted_rows))
}

/// Lexer state for [`rewrite_placeholders`].
///
/// Only [`ScanState::Normal`] recognizes placeholders. Every other state
/// preserves marker-like text inside a quoted token or comment.
#[derive(Clone, Copy, Eq, PartialEq)]
enum ScanState {
    /// Executable SQL where parameter markers may be rewritten.
    Normal,
    /// A single-quoted string, where `''` escapes a quote.
    SingleQuote,
    /// A double-quoted token, where `""` escapes a quote.
    DoubleQuote,
    /// A bracketed identifier, where `]]` escapes a closing bracket.
    Bracket,
    /// A `--` comment ending at either `\r` or `\n`.
    LineComment,
    /// A nested `/* ... */` comment tracked by `block_comment_depth`.
    BlockComment,
}

/// A rewritten parameter marker in RPC wire order.
struct Placeholder {
    /// The generated collision-free name used in both SQL text and RPC metadata.
    rpc_name: String,
    /// The dictionary key for `%(name)s`, or `None` for positional `?`.
    source_name: Option<String>,
}

fn parse_pyformat_marker(sql: &str, start: usize, close: Option<usize>) -> Option<(&str, usize)> {
    let body_start = start.checked_add("%(".len())?;
    let close = close.filter(|close| *close >= body_start)?;
    sql.get(close + 1..)?.strip_prefix('s')?;
    let source_name = sql.get(body_start..close)?;
    Some((source_name, close + ")s".len() - start))
}

fn rpc_prefix(sql: &str) -> String {
    let lowercase_sql = sql.to_ascii_lowercase();
    (0_u32..)
        .map(|suffix| format!("@__mssql_py_{suffix}_"))
        .find(|candidate| !lowercase_sql.contains(candidate))
        .expect("an unused generated RPC prefix exists")
}

/// Normalizes Python execute arguments and binds them as positional or named RPC parameters.
///
/// A sole dictionary selects `%(name)s` binding. A sole tuple, list, or DB-API
/// `Row` is expanded positionally; all other argument tuples bind positionally
/// as supplied. The returned metadata is in placeholder occurrence order.
pub(crate) fn bind_parameters(
    operation: String,
    parameters: &Bound<'_, PyTuple>,
    hints: Option<&[ParameterHint]>,
) -> PyResult<(String, Vec<RpcParameter>, Vec<ParameterMetadata>)> {
    if parameters.len() == 1 {
        let parameter = parameters.get_item(0)?;
        if let Ok(values) = parameter.cast::<PyDict>() {
            return bind_named(operation, values, hints);
        }
        if let Ok(values) = parameter.cast::<PyTuple>() {
            return bind_positional(operation, values.iter(), hints);
        }
        if let Ok(values) = parameter.cast::<PyList>() {
            return bind_positional(operation, values.iter(), hints);
        }
        if parameter.get_type().name()? == "Row" {
            let values = parameter.try_iter()?.collect::<PyResult<Vec<_>>>()?;
            return bind_positional(operation, values.into_iter(), hints);
        }
    }

    bind_positional(operation, parameters.iter(), hints)
}

/// Rewritten SQL and placeholder metadata reused for every ExecuteMany row.
pub(crate) struct ParameterBindingPlan {
    operation: String,
    placeholders: Vec<Placeholder>,
    named: bool,
}

impl ParameterBindingPlan {
    pub(crate) fn new(operation: &str, named: bool) -> PyResult<Self> {
        let (operation, placeholders) = rewrite_placeholders(operation, named)?;
        Ok(Self {
            operation,
            placeholders,
            named,
        })
    }

    pub(crate) fn operation(&self) -> &str {
        &self.operation
    }

    pub(crate) fn bind_row(
        &self,
        row: &Bound<'_, PyAny>,
        hints: Option<&[ParameterHint]>,
    ) -> PyResult<(Vec<RpcParameter>, Vec<ParameterMetadata>)> {
        validate_hint_count(hints, self.placeholders.len())?;
        if self.named {
            let values = row.cast::<PyDict>()?;
            if self.placeholders.is_empty() && !values.is_empty() {
                return Err(PyTypeError::new_err(format!(
                    "The SQL contains no parameter markers, but {} parameters were supplied. \
                     Named parameters use the %(name)s style.",
                    values.len()
                )));
            }
            let bound = self
                .placeholders
                .iter()
                .enumerate()
                .map(|(index, placeholder)| {
                    let source_name = placeholder
                        .source_name
                        .as_ref()
                        .expect("named placeholders include source names");
                    let value = values
                        .get_item(source_name)?
                        .ok_or_else(|| PyKeyError::new_err(source_name.clone()))?;
                    rpc_parameter(
                        placeholder.rpc_name.clone(),
                        &value,
                        hints.and_then(|hints| hints.get(index)),
                    )
                })
                .collect::<PyResult<Vec<_>>>()?;
            return Ok(bound.into_iter().unzip());
        }

        let values = row.try_iter()?.collect::<PyResult<Vec<_>>>()?;
        if self.placeholders.len() != values.len() {
            return Err(PyTypeError::new_err(format!(
                "The SQL contains {} parameter markers, but {} parameters were supplied",
                self.placeholders.len(),
                values.len()
            )));
        }
        let bound = self
            .placeholders
            .iter()
            .zip(values)
            .enumerate()
            .map(|(index, (placeholder, value))| {
                rpc_parameter(
                    placeholder.rpc_name.clone(),
                    &value,
                    hints.and_then(|hints| hints.get(index)),
                )
            })
            .collect::<PyResult<Vec<_>>>()?;
        Ok(bound.into_iter().unzip())
    }
}

/// Parses `setinputsizes()` entries into validated conversion hints.
///
/// Each entry may be a SQL type integer or `(sql_type[, size[, scale]])`.
/// `None` and an empty iterable both produce no hints.
pub(crate) fn parse_input_sizes(sizes: &Bound<'_, PyAny>) -> PyResult<Option<Vec<ParameterHint>>> {
    if sizes.is_none() {
        return Ok(None);
    }
    let mut hints = Vec::new();
    for item in sizes.try_iter()? {
        let item = item?;
        let (sql_type, size, scale) = if let Ok(values) = item.cast::<PyTuple>() {
            if values.is_empty() || values.len() > 3 {
                return Err(PyValueError::new_err(
                    "Each input size tuple must contain (sql_type[, size[, decimal_digits]])",
                ));
            }
            let sql_type = values.get_item(0)?.extract::<i32>()?;
            let size = if values.len() >= 2 {
                values.get_item(1)?.extract::<u32>()?
            } else {
                0
            };
            let scale = if values.len() == 3 {
                values.get_item(2)?.extract::<u8>()?
            } else {
                0
            };
            (sql_type, size, scale)
        } else {
            (item.extract::<i32>()?, 0, 0)
        };
        hints.push(
            ParameterHint::new(sql_type, size, scale)
                .map_err(|error| PyValueError::new_err(error.to_string()))?,
        );
    }
    Ok((!hints.is_empty()).then_some(hints))
}

/// Rewrites and binds positional values in `?` occurrence order.
fn bind_positional<'py>(
    operation: String,
    values: impl Iterator<Item = Bound<'py, PyAny>>,
    hints: Option<&[ParameterHint]>,
) -> PyResult<(String, Vec<RpcParameter>, Vec<ParameterMetadata>)> {
    let values = values.collect::<Vec<_>>();
    let (operation, names) = rewrite_placeholders(&operation, false)?;
    if names.len() != values.len() {
        return Err(PyTypeError::new_err(format!(
            "The SQL contains {} parameter markers, but {} parameters were supplied",
            names.len(),
            values.len()
        )));
    }
    validate_hint_count(hints, names.len())?;
    let bound_parameters = names
        .into_iter()
        .zip(values)
        .enumerate()
        .map(|(index, (placeholder, value))| {
            rpc_parameter(
                placeholder.rpc_name,
                &value,
                hints.and_then(|hints| hints.get(index)),
            )
        })
        .collect::<PyResult<Vec<_>>>()?;
    let (parameters, parameter_signature) = bound_parameters.into_iter().unzip();
    Ok((operation, parameters, parameter_signature))
}

/// Rewrites `%(name)s` markers and looks up each value by dictionary key.
fn bind_named(
    operation: String,
    values: &Bound<'_, PyDict>,
    hints: Option<&[ParameterHint]>,
) -> PyResult<(String, Vec<RpcParameter>, Vec<ParameterMetadata>)> {
    let (operation, names) = rewrite_placeholders(&operation, true)?;
    if names.is_empty() && !values.is_empty() {
        return Err(PyTypeError::new_err(format!(
            "The SQL contains no parameter markers, but {} parameters were supplied. \
             Named parameters use the %(name)s style.",
            values.len()
        )));
    }
    validate_hint_count(hints, names.len())?;
    let bound_parameters = names
        .into_iter()
        .enumerate()
        .map(|(index, placeholder)| {
            let source_name = placeholder
                .source_name
                .expect("named placeholders include source names");
            let value = values
                .get_item(&source_name)?
                .ok_or_else(|| PyKeyError::new_err(source_name.clone()))?;
            rpc_parameter(
                placeholder.rpc_name,
                &value,
                hints.and_then(|hints| hints.get(index)),
            )
        })
        .collect::<PyResult<Vec<_>>>()?;
    let (parameters, parameter_signature) = bound_parameters.into_iter().unzip();
    Ok((operation, parameters, parameter_signature))
}

fn validate_hint_count(hints: Option<&[ParameterHint]>, parameter_count: usize) -> PyResult<()> {
    if let Some(hints) = hints
        && hints.len() != parameter_count
    {
        return Err(PyTypeError::new_err(format!(
            "setinputsizes contains {} hints, but the SQL contains {parameter_count} parameter markers",
            hints.len()
        )));
    }
    Ok(())
}

/// Converts one Python value into an RPC parameter and its declaration metadata.
///
/// TVPs reject `setinputsizes()` hints and use the default-value status for a
/// NULL table. Scalar conversion failures are exposed as Python type errors.
fn rpc_parameter(
    name: String,
    value: &Bound<'_, PyAny>,
    hint: Option<&ParameterHint>,
) -> PyResult<(RpcParameter, ParameterMetadata)> {
    // TODO(mssql-tds): Expose general parameter description backed by
    // sp_describe_undeclared_parameters. The existing private path is specific
    // to Always Encrypted, so an unhinted Python None falls back to NVARCHAR(1).
    // TODO(performance): Benchmark representative scalar binding conversions.
    let (value, status) = if let Ok(tvp) = value.extract::<PyRef<'_, PyTableValuedParameter>>() {
        if hint.is_some() {
            return Err(PyTypeError::new_err(
                "setinputsizes hints cannot be applied to a TableValuedParameter",
            ));
        }
        let status = if tvp.is_null() {
            StatusFlags::DEFAULT_VALUE
        } else {
            StatusFlags::NONE
        };
        (tvp.sql_type(), status)
    } else {
        (
            match hint {
                Some(hint) => py_to_sql_type_with_hint(value, *hint),
                None => py_to_sql_type(value),
            }
            .map_err(|error| PyTypeError::new_err(error.to_string()))?,
            StatusFlags::NONE,
        )
    };
    let signature = sql_type_metadata(&value);
    Ok((RpcParameter::new(Some(name), status, value), signature))
}

/// Rewrites DB-API parameter markers to collision-free SQL Server RPC names.
///
/// Positional mode recognizes `?`; dictionary-backed named mode recognizes
/// `%(name)s` and converts `%%` to `%`. A valid marker from the other mode is
/// rejected as a style mismatch. Markers are rewritten only in
/// [`ScanState::Normal`], while quoted tokens and comments are preserved.
/// Malformed pyformat candidates and unsupported `:name` markers are left
/// unchanged for the caller's marker-count validation.
fn rewrite_placeholders(sql: &str, named: bool) -> PyResult<(String, Vec<Placeholder>)> {
    // TODO(performance): Benchmark placeholder scanning. If results justify
    // caching, share rewritten SQL and marker metadata at connection scope,
    // use borrowed SQL plus parameter style for hit-path lookup, and bound the
    // retained bytes.
    let mut output = String::with_capacity(sql.len() + sql.len() / 2);
    let mut names = Vec::new();
    let rpc_prefix = rpc_prefix(sql);
    let mut state = ScanState::Normal;
    let mut block_comment_depth = 0usize;
    let mut chars = sql.char_indices().peekable();
    let mut close_parentheses = sql.match_indices(')').map(|(offset, _)| offset).peekable();

    while let Some((index, current)) = chars.next() {
        let next = chars.peek().map(|(_, next)| *next);
        match state {
            ScanState::Normal => match (current, next) {
                ('\'', _) => {
                    state = ScanState::SingleQuote;
                    output.push(current);
                }
                ('"', _) => {
                    state = ScanState::DoubleQuote;
                    output.push(current);
                }
                ('[', _) => {
                    state = ScanState::Bracket;
                    output.push(current);
                }
                ('-', Some('-')) => {
                    state = ScanState::LineComment;
                    output.push_str("--");
                    chars.next();
                }
                ('/', Some('*')) => {
                    state = ScanState::BlockComment;
                    block_comment_depth = 1;
                    output.push_str("/*");
                    chars.next();
                }
                ('?', _) if !named => {
                    let rpc_name = format!("{rpc_prefix}{}", names.len() + 1);
                    output.push_str(&rpc_name);
                    names.push(Placeholder {
                        rpc_name,
                        source_name: None,
                    });
                }
                ('?', _) => {
                    return Err(PyTypeError::new_err(
                        "Parameter style mismatch: query uses positional placeholders (?) but a dict was provided",
                    ));
                }
                ('%', Some('%')) if named => {
                    output.push('%');
                    chars.next();
                }
                ('%', Some('(')) => {
                    let body_start = index + "%(".len();
                    while close_parentheses
                        .peek()
                        .is_some_and(|close| *close < body_start)
                    {
                        close_parentheses.next();
                    }
                    if let Some((source_name, consumed)) =
                        parse_pyformat_marker(sql, index, close_parentheses.peek().copied())
                    {
                        if !named {
                            return Err(PyTypeError::new_err(
                                "Parameter style mismatch: query uses named placeholders (%(name)s) but positional parameters were provided",
                            ));
                        }
                        if source_name.is_empty() {
                            return Err(PyTypeError::new_err("Named parameter cannot be empty"));
                        }
                        let rpc_name = format!("{rpc_prefix}{}", names.len() + 1);
                        output.push_str(&rpc_name);
                        names.push(Placeholder {
                            rpc_name,
                            source_name: Some(source_name.to_owned()),
                        });
                        let end = index + consumed;
                        while chars.peek().is_some_and(|(offset, _)| *offset < end) {
                            chars.next();
                        }
                    } else {
                        output.push(current);
                    }
                }
                _ => output.push(current),
            },
            ScanState::SingleQuote => {
                output.push(current);
                if current == '\'' {
                    if next == Some('\'') {
                        output.push('\'');
                        chars.next();
                    } else {
                        state = ScanState::Normal;
                    }
                }
            }
            ScanState::DoubleQuote => {
                output.push(current);
                if current == '"' {
                    if next == Some('"') {
                        output.push('"');
                        chars.next();
                    } else {
                        state = ScanState::Normal;
                    }
                }
            }
            ScanState::Bracket => {
                output.push(current);
                if current == ']' {
                    if next == Some(']') {
                        output.push(']');
                        chars.next();
                    } else {
                        state = ScanState::Normal;
                    }
                }
            }
            ScanState::LineComment => {
                output.push(current);
                if current == '\n' || current == '\r' {
                    state = ScanState::Normal;
                }
            }
            ScanState::BlockComment => {
                if current == '/' && next == Some('*') {
                    output.push_str("/*");
                    chars.next();
                    block_comment_depth += 1;
                } else if current == '*' && next == Some('/') {
                    output.push_str("*/");
                    chars.next();
                    block_comment_depth -= 1;
                    if block_comment_depth == 0 {
                        state = ScanState::Normal;
                    }
                } else {
                    output.push(current);
                }
            }
        }
    }

    Ok((output, names))
}

#[cfg(test)]
mod tests {
    use super::rewrite_placeholders;

    #[test]
    fn rewrites_only_executable_qmarks() {
        let sql = "SELECT ?, '?', [??] -- ?\n/* ? */ WHERE value = ?";
        let (sql, names) = rewrite_placeholders(sql, false).unwrap();
        assert_eq!(
            sql,
            "SELECT @__mssql_py_0_1, '?', [??] -- ?\n/* ? */ WHERE value = @__mssql_py_0_2"
        );
        assert_eq!(
            names
                .into_iter()
                .map(|placeholder| placeholder.rpc_name)
                .collect::<Vec<_>>(),
            vec!["@__mssql_py_0_1", "@__mssql_py_0_2"]
        );
    }

    #[test]
    fn line_comment_ends_at_carriage_return() {
        let (sql, names) = rewrite_placeholders("SELECT 1 -- c\rWHERE a = ?", false).unwrap();

        assert_eq!(sql, "SELECT 1 -- c\rWHERE a = @__mssql_py_0_1");
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn line_comment_still_ends_at_crlf() {
        let (sql, names) = rewrite_placeholders("SELECT 1 -- c\r\nWHERE a = ?", false).unwrap();

        assert_eq!(sql, "SELECT 1 -- c\r\nWHERE a = @__mssql_py_0_1");
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn leading_line_comment_ends_at_carriage_return() {
        let (sql, names) = rewrite_placeholders("-- lead\rSELECT ?, ?", false).unwrap();

        assert_eq!(sql, "-- lead\rSELECT @__mssql_py_0_1, @__mssql_py_0_2");
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn rewrites_dense_positional_parameters() {
        let sql = (0..2000).map(|_| "?").collect::<Vec<_>>().join(", ");
        let (sql, names) = rewrite_placeholders(&sql, false).unwrap();

        assert_eq!(names.len(), 2000);
        assert_eq!(names.first().unwrap().rpc_name, "@__mssql_py_0_1");
        assert_eq!(names.last().unwrap().rpc_name, "@__mssql_py_0_2000");
        assert_eq!(sql.split(", ").count(), 2000);
        assert!(sql.starts_with("@__mssql_py_0_1, @__mssql_py_0_2"));
        assert!(sql.ends_with("@__mssql_py_0_1999, @__mssql_py_0_2000"));
    }

    #[test]
    fn rewrites_named_parameters_in_occurrence_order() {
        let (sql, names) = rewrite_placeholders("SELECT %(id)s, %(id)s, %(name)s", true).unwrap();
        assert_eq!(
            sql,
            "SELECT @__mssql_py_0_1, @__mssql_py_0_2, @__mssql_py_0_3"
        );
        assert_eq!(
            names
                .into_iter()
                .map(|placeholder| (placeholder.rpc_name, placeholder.source_name.unwrap()))
                .collect::<Vec<_>>(),
            vec![
                ("@__mssql_py_0_1".to_string(), "id".to_string()),
                ("@__mssql_py_0_2".to_string(), "id".to_string()),
                ("@__mssql_py_0_3".to_string(), "name".to_string()),
            ]
        );
    }

    #[test]
    fn generated_names_do_not_collide_with_existing_parameter_names() {
        let sql = "DECLARE @P1 int = 10; SELECT @P1, ?, ?";
        let (sql, names) = rewrite_placeholders(sql, false).unwrap();

        assert_eq!(
            sql,
            "DECLARE @P1 int = 10; SELECT @P1, @__mssql_py_0_1, @__mssql_py_0_2"
        );
        assert_eq!(names[0].rpc_name, "@__mssql_py_0_1");
        assert_eq!(names[1].rpc_name, "@__mssql_py_0_2");
    }

    #[test]
    fn generated_prefix_collision_check_is_case_insensitive() {
        let sql = "DECLARE @__MsSqL_Py_0_1 int; SELECT %(first)s, %(second)s";
        let (sql, names) = rewrite_placeholders(sql, true).unwrap();

        assert_eq!(
            sql,
            "DECLARE @__MsSqL_Py_0_1 int; SELECT @__mssql_py_1_1, @__mssql_py_1_2"
        );
        assert_eq!(names[0].rpc_name, "@__mssql_py_1_1");
        assert_eq!(names[1].rpc_name, "@__mssql_py_1_2");
    }

    #[test]
    fn rewrites_named_parameters_with_unicode() {
        let (sql, names) = rewrite_placeholders("SELECT N'東京', %(café)s", true).unwrap();

        assert_eq!(sql, "SELECT N'東京', @__mssql_py_0_1");
        assert_eq!(names[0].source_name.as_deref(), Some("café"));
    }

    #[test]
    fn preserves_markers_in_escaped_quoted_contexts() {
        let sql = "SELECT 'it''s ?', \"col\"\"?\", [col]]?] WHERE value = ?";
        let (sql, names) = rewrite_placeholders(sql, false).unwrap();

        assert_eq!(
            sql,
            "SELECT 'it''s ?', \"col\"\"?\", [col]]?] WHERE value = @__mssql_py_0_1"
        );
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn preserves_markers_in_unterminated_contexts() {
        for sql in [
            "SELECT 'unterminated ?",
            "SELECT \"unterminated ?",
            "SELECT [unterminated ?",
            "SELECT /* unterminated ?",
        ] {
            let (rewritten, names) = rewrite_placeholders(sql, false).unwrap();
            assert_eq!(rewritten, sql);
            assert!(names.is_empty());
        }
    }

    #[test]
    fn preserves_malformed_named_markers_and_rewrites_later_valid_markers() {
        let (sql, names) =
            rewrite_placeholders("SELECT %(broken)x, %(value)s, %(unterminated", true).unwrap();

        assert_eq!(sql, "SELECT %(broken)x, @__mssql_py_0_1, %(unterminated");
        assert_eq!(names.len(), 1);
        assert_eq!(names[0].source_name.as_deref(), Some("value"));
    }

    #[test]
    fn rewrites_marker_inside_a_modulo_parenthesis() {
        let (sql, names) =
            rewrite_placeholders("SELECT a %(SELECT c FROM t WHERE id = ?) FROM u", false).unwrap();

        assert_eq!(
            sql,
            "SELECT a %(SELECT c FROM t WHERE id = @__mssql_py_0_1) FROM u"
        );
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn unterminated_percent_paren_does_not_swallow_later_markers() {
        let (sql, names) = rewrite_placeholders("SELECT a %(b FROM t WHERE c = ?", false).unwrap();

        assert_eq!(sql, "SELECT a %(b FROM t WHERE c = @__mssql_py_0_1");
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn quoting_is_tracked_after_a_bare_percent_paren() {
        let (sql, names) = rewrite_placeholders("SELECT a %(b, '?' , ? FROM t", false).unwrap();

        assert_eq!(sql, "SELECT a %(b, '?' , @__mssql_py_0_1 FROM t");
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn repeated_malformed_percent_parens_do_not_hide_a_later_marker() {
        let sql = format!("SELECT {} ?", "%(".repeat(2000));
        let (rewritten, names) = rewrite_placeholders(&sql, false).unwrap();

        assert!(rewritten.ends_with("@__mssql_py_0_1"));
        assert_eq!(names.len(), 1);
    }

    #[test]
    fn restores_escaped_percent_for_named_parameters() {
        let (sql, names) =
            rewrite_placeholders("SELECT 100%%, %(value)s, '%%(ignored)s'", true).unwrap();

        assert_eq!(sql, "SELECT 100%, @__mssql_py_0_1, '%%(ignored)s'");
        assert_eq!(names.len(), 1);
        assert_eq!(names[0].source_name.as_deref(), Some("value"));
    }

    #[test]
    fn rejects_positional_markers_with_named_parameters() {
        let error = match rewrite_placeholders("SELECT [q?mark], ?", true) {
            Ok(_) => panic!("expected positional marker rejection"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("positional placeholders"));
    }

    #[test]
    fn rejects_named_markers_with_positional_parameters() {
        let error = match rewrite_placeholders("SELECT %(value)s", false) {
            Ok(_) => panic!("expected named marker rejection"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("named placeholders"));
    }

    #[test]
    fn preserves_markers_in_nested_block_comments() {
        let sql = "SELECT /* outer /* inner */ ? still outer */ ?";
        let (sql, names) = rewrite_placeholders(sql, false).unwrap();

        assert_eq!(
            sql,
            "SELECT /* outer /* inner */ ? still outer */ @__mssql_py_0_1"
        );
        assert_eq!(names.len(), 1);
    }

    #[test]
    #[ignore = "sync compatibility gap: empty pyformat parameter names are rejected"]
    fn accepts_empty_named_parameter() {
        let (sql, names) = rewrite_placeholders("SELECT %()s", true).unwrap();

        assert_eq!(sql, "SELECT @__mssql_py_0_1");
        assert_eq!(names[0].source_name.as_deref(), Some(""));
    }
}
