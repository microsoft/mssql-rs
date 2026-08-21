// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use mssql_tds::datatypes::sql_tvp::{TvpColumnDef, TvpTableData, TvpTypeName};
use mssql_tds::datatypes::sqldatatypes::VectorBaseType;
use mssql_tds::datatypes::sqltypes::SqlType;
use mssql_tds::message::parameters::rpc_parameters::{RpcParameter, StatusFlags};
use pyo3::exceptions::{PyKeyError, PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyTuple};
use std::fmt::Write as _;

use crate::types::{ParameterHint, null_sql_type, py_to_sql_type, py_to_sql_type_with_hint};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ParameterMetadata {
    Scalar(&'static str),
    Decimal {
        numeric: bool,
        precision: u8,
        scale: u8,
    },
    Temporal {
        kind: &'static str,
        scale: u8,
    },
    Sized {
        kind: &'static str,
        length: u16,
    },
    Vector {
        dimensions: u16,
        base_type: VectorBaseType,
    },
    Variant(Box<ParameterMetadata>),
    Table {
        schema: String,
        name: String,
    },
}

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

#[pyclass(name = "TableValuedParameter", frozen)]
pub(crate) struct PyTableValuedParameter {
    type_name: TvpTypeName,
    table: Option<TvpTableData>,
}

#[pymethods]
impl PyTableValuedParameter {
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

    #[getter]
    fn type_name(&self) -> &str {
        &self.type_name.type_name
    }

    #[getter]
    fn schema(&self) -> Option<&str> {
        self.type_name.schema_name.as_deref()
    }

    #[getter]
    fn is_null(&self) -> bool {
        self.table.is_none()
    }

    #[getter]
    fn column_count(&self) -> usize {
        self.table.as_ref().map_or(0, |table| table.columns.len())
    }

    #[getter]
    fn row_count(&self) -> usize {
        self.table.as_ref().map_or(0, |table| table.rows.len())
    }
}

impl PyTableValuedParameter {
    fn sql_type(&self) -> SqlType {
        // TODO: Use shared TVP ownership when mssql-tds supports Arc<TvpTableData>.
        SqlType::Table(self.type_name.clone(), self.table.clone())
    }
}

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

fn build_tvp_table(columns: &Bound<'_, PyAny>, rows: &Bound<'_, PyAny>) -> PyResult<TvpTableData> {
    let hints = parse_input_sizes(columns)?
        .ok_or_else(|| PyValueError::new_err("A TVP must define at least one column"))?;
    let mut column_defs = Vec::with_capacity(hints.len());
    for hint in &hints {
        if matches!(hint.sql_type(), -151 | -1 | -10) {
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

#[derive(Clone, Copy, Eq, PartialEq)]
enum ScanState {
    Normal,
    SingleQuote,
    DoubleQuote,
    Bracket,
    LineComment,
    BlockComment,
}

struct Placeholder {
    rpc_name: String,
    source_name: Option<String>,
}

pub(crate) fn bind_parameters(
    operation: String,
    parameters: &Bound<'_, PyTuple>,
    hints: Option<&[ParameterHint]>,
) -> PyResult<(String, Vec<RpcParameter>, Vec<ParameterMetadata>)> {
    if parameters.is_empty() {
        return Ok((operation, Vec::new(), Vec::new()));
    }

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

fn rpc_parameter(
    name: String,
    value: &Bound<'_, PyAny>,
    hint: Option<&ParameterHint>,
) -> PyResult<(RpcParameter, ParameterMetadata)> {
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

fn rewrite_placeholders(sql: &str, named: bool) -> PyResult<(String, Vec<Placeholder>)> {
    let mut output = String::with_capacity(sql.len() + sql.len() / 2);
    let mut names = Vec::new();
    let mut state = ScanState::Normal;
    let mut block_comment_depth = 0usize;
    let mut chars = sql.char_indices().peekable();

    while let Some((_, current)) = chars.next() {
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
                    let start = output.len();
                    write!(&mut output, "@P{}", names.len() + 1)
                        .expect("writing to a String cannot fail");
                    let rpc_name = output[start..].to_owned();
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
                    chars.next();
                    let mut source_name = String::new();
                    let mut original = String::from("%(");
                    let mut complete = false;
                    while let Some((_, marker_char)) = chars.next() {
                        original.push(marker_char);
                        if marker_char == ')' {
                            if chars.peek().is_some_and(|(_, next)| *next == 's') {
                                chars.next();
                                original.push('s');
                                complete = true;
                            }
                            break;
                        }
                        source_name.push(marker_char);
                    }
                    if complete {
                        if !named {
                            return Err(PyTypeError::new_err(
                                "Parameter style mismatch: query uses named placeholders (%(name)s) but positional parameters were provided",
                            ));
                        }
                        if source_name.is_empty() {
                            return Err(PyTypeError::new_err("Named parameter cannot be empty"));
                        }
                        let start = output.len();
                        write!(&mut output, "@P{}", names.len() + 1)
                            .expect("writing to a String cannot fail");
                        let rpc_name = output[start..].to_owned();
                        names.push(Placeholder {
                            rpc_name,
                            source_name: Some(source_name),
                        });
                    } else {
                        output.push_str(&original);
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
                if current == '\n' {
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
        assert_eq!(sql, "SELECT @P1, '?', [??] -- ?\n/* ? */ WHERE value = @P2");
        assert_eq!(
            names
                .into_iter()
                .map(|placeholder| placeholder.rpc_name)
                .collect::<Vec<_>>(),
            vec!["@P1", "@P2"]
        );
    }

    #[test]
    fn rewrites_dense_positional_parameters() {
        let sql = (0..2000).map(|_| "?").collect::<Vec<_>>().join(", ");
        let (sql, names) = rewrite_placeholders(&sql, false).unwrap();

        assert_eq!(names.len(), 2000);
        assert_eq!(names.first().unwrap().rpc_name, "@P1");
        assert_eq!(names.last().unwrap().rpc_name, "@P2000");
        assert_eq!(sql.split(", ").count(), 2000);
        assert!(sql.starts_with("@P1, @P2"));
        assert!(sql.ends_with("@P1999, @P2000"));
    }

    #[test]
    fn rewrites_named_parameters_in_occurrence_order() {
        let (sql, names) = rewrite_placeholders("SELECT %(id)s, %(id)s, %(name)s", true).unwrap();
        assert_eq!(sql, "SELECT @P1, @P2, @P3");
        assert_eq!(
            names
                .into_iter()
                .map(|placeholder| (placeholder.rpc_name, placeholder.source_name.unwrap()))
                .collect::<Vec<_>>(),
            vec![
                ("@P1".to_string(), "id".to_string()),
                ("@P2".to_string(), "id".to_string()),
                ("@P3".to_string(), "name".to_string()),
            ]
        );
    }

    #[test]
    fn rewrites_named_parameters_with_unicode() {
        let (sql, names) = rewrite_placeholders("SELECT N'東京', %(café)s", true).unwrap();

        assert_eq!(sql, "SELECT N'東京', @P1");
        assert_eq!(names[0].source_name.as_deref(), Some("café"));
    }

    #[test]
    fn preserves_markers_in_escaped_quoted_contexts() {
        let sql = "SELECT 'it''s ?', \"col\"\"?\", [col]]?] WHERE value = ?";
        let (sql, names) = rewrite_placeholders(sql, false).unwrap();

        assert_eq!(
            sql,
            "SELECT 'it''s ?', \"col\"\"?\", [col]]?] WHERE value = @P1"
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

        assert_eq!(sql, "SELECT %(broken)x, @P1, %(unterminated");
        assert_eq!(names.len(), 1);
        assert_eq!(names[0].source_name.as_deref(), Some("value"));
    }

    #[test]
    fn restores_escaped_percent_for_named_parameters() {
        let (sql, names) =
            rewrite_placeholders("SELECT 100%%, %(value)s, '%%(ignored)s'", true).unwrap();

        assert_eq!(sql, "SELECT 100%, @P1, '%%(ignored)s'");
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

        assert_eq!(sql, "SELECT /* outer /* inner */ ? still outer */ @P1");
        assert_eq!(names.len(), 1);
    }

    #[test]
    #[ignore = "sync compatibility gap: empty pyformat parameter names are rejected"]
    fn accepts_empty_named_parameter() {
        let (sql, names) = rewrite_placeholders("SELECT %()s", true).unwrap();

        assert_eq!(sql, "SELECT @P1");
        assert_eq!(names[0].source_name.as_deref(), Some(""));
    }
}
