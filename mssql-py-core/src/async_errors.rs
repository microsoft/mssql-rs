// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! DB-API exception mapping for asynchronous TDS operations.

use mssql_tds::error::{Error as TdsError, SqlErrorInfo, SqlInfoMessage, SqlServerDiagnostics};
use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList};

create_exception!(mssql_py_core, Error, PyException);
create_exception!(mssql_py_core, Warning, PyException);
create_exception!(mssql_py_core, InterfaceError, Error);
create_exception!(mssql_py_core, DatabaseError, Error);
create_exception!(mssql_py_core, DataError, DatabaseError);
create_exception!(mssql_py_core, OperationalError, DatabaseError);
create_exception!(mssql_py_core, IntegrityError, DatabaseError);
create_exception!(mssql_py_core, InternalError, DatabaseError);
create_exception!(mssql_py_core, ProgrammingError, DatabaseError);
create_exception!(mssql_py_core, NotSupportedError, DatabaseError);

pub(crate) fn add_exceptions(module: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = module.py();
    module.add("Error", py.get_type::<Error>())?;
    module.add("Warning", py.get_type::<Warning>())?;
    module.add("InterfaceError", py.get_type::<InterfaceError>())?;
    module.add("DatabaseError", py.get_type::<DatabaseError>())?;
    module.add("DataError", py.get_type::<DataError>())?;
    module.add("OperationalError", py.get_type::<OperationalError>())?;
    module.add("IntegrityError", py.get_type::<IntegrityError>())?;
    module.add("InternalError", py.get_type::<InternalError>())?;
    module.add("ProgrammingError", py.get_type::<ProgrammingError>())?;
    module.add("NotSupportedError", py.get_type::<NotSupportedError>())?;
    Ok(())
}

pub(crate) fn map_tds_error(
    context: &str,
    error: TdsError,
    statement_info_messages: Vec<SqlInfoMessage>,
) -> PyErr {
    let message = format!("{context}: {error}");
    match error {
        TdsError::SqlServerError { mut diagnostics } => {
            diagnostics.info_messages.extend(statement_info_messages);
            database_error_with_diagnostics(message, diagnostics)
        }
        TdsError::UsageError(_) => ProgrammingError::new_err(message),
        TdsError::TypeConversionError(_)
        | TdsError::UnsupportedEncoding { .. }
        | TdsError::ColumnEncryptionError(_) => DataError::new_err(message),
        TdsError::UnimplementedFeature { .. } => NotSupportedError::new_err(message),
        TdsError::ProtocolError(_)
        | TdsError::ImplementationError(_)
        | TdsError::ConnectionResetNotAcknowledged => InternalError::new_err(message),
        TdsError::Io(_)
        | TdsError::Redirection { .. }
        | TdsError::ConnectionError(_)
        | TdsError::TlsError(_)
        | TdsError::TlsHandshakeError { .. }
        | TdsError::TimeoutError(_)
        | TdsError::OperationCancelledError(_)
        | TdsError::ConnectionClosed(_)
        | TdsError::CertificateNotFound { .. }
        | TdsError::InvalidCertificateFormat { .. }
        | TdsError::CertificateExpired
        | TdsError::CertificateMismatch
        | TdsError::CertificateFileIoError { .. }
        | TdsError::NoServerCertificate
        | TdsError::BulkCopyError(_)
        | TdsError::Security(_)
        | TdsError::SessionRecoveryFailed { .. }
        | TdsError::SessionNotRecoverable(_)
        | TdsError::ReconnectionValidationFailed(_) => OperationalError::new_err(message),
    }
}

fn database_error_with_diagnostics(message: String, diagnostics: SqlServerDiagnostics) -> PyErr {
    let error = DatabaseError::new_err(message);
    Python::attach(|py| {
        let value = error.value(py);
        let errors = diagnostics
            .errors
            .iter()
            .map(|item| sql_error_dict(py, item))
            .collect::<PyResult<Vec<_>>>()?;
        let info_messages = diagnostics
            .info_messages
            .iter()
            .map(|item| sql_info_dict(py, item))
            .collect::<PyResult<Vec<_>>>()?;
        value.setattr("sql_errors", PyList::new(py, errors)?)?;
        value.setattr("info_messages", PyList::new(py, info_messages)?)?;
        Ok::<(), PyErr>(())
    })
    .unwrap_or_else(|attribute_error| {
        tracing::warn!(
            "Failed to attach SQL Server diagnostics to Python exception: {attribute_error}"
        );
    });
    error
}

fn sql_error_dict<'py>(py: Python<'py>, item: &SqlErrorInfo) -> PyResult<Bound<'py, PyDict>> {
    diagnostic_dict(
        py,
        &item.message,
        item.state,
        item.class,
        item.number,
        item.server_name.as_deref(),
        item.proc_name.as_deref(),
        item.line_number,
    )
}

fn sql_info_dict<'py>(py: Python<'py>, item: &SqlInfoMessage) -> PyResult<Bound<'py, PyDict>> {
    diagnostic_dict(
        py,
        &item.message,
        item.state,
        item.class,
        item.number,
        item.server_name.as_deref(),
        item.proc_name.as_deref(),
        item.line_number,
    )
}

#[allow(clippy::too_many_arguments)]
fn diagnostic_dict<'py>(
    py: Python<'py>,
    message: &str,
    state: u8,
    class: i32,
    number: u32,
    server_name: Option<&str>,
    proc_name: Option<&str>,
    line_number: Option<i32>,
) -> PyResult<Bound<'py, PyDict>> {
    let diagnostic = PyDict::new(py);
    diagnostic.set_item("message", message)?;
    diagnostic.set_item("state", state)?;
    diagnostic.set_item("class", class)?;
    diagnostic.set_item("number", number)?;
    diagnostic.set_item("server_name", server_name)?;
    diagnostic.set_item("proc_name", proc_name)?;
    diagnostic.set_item("line_number", line_number)?;
    Ok(diagnostic)
}

#[cfg(test)]
mod tests {
    use mssql_tds::error::{Error as TdsError, SqlErrorInfo, SqlInfoMessage, SqlServerDiagnostics};
    use pyo3::types::PyAnyMethods;
    use pyo3::{Py, PyAny, Python};

    use super::{
        DataError, DatabaseError, InternalError, NotSupportedError, OperationalError,
        ProgrammingError, map_tds_error,
    };

    #[test]
    fn maps_timeout_to_operational_error() {
        let error = map_tds_error(
            "PyAsyncCursor.fetchone failed while reading rows",
            TdsError::TimeoutError(mssql_tds::error::TimeoutErrorType::String(
                "deadline exceeded".to_string(),
            )),
            Vec::new(),
        );

        Python::attach(|py| assert!(error.is_instance_of::<OperationalError>(py)));
        assert!(error.to_string().contains("deadline exceeded"));
    }

    #[test]
    fn maps_client_error_categories() {
        Python::attach(|py| {
            let usage = map_tds_error(
                "operation failed",
                TdsError::UsageError("usage".into()),
                Vec::new(),
            );
            assert!(usage.is_instance_of::<ProgrammingError>(py));

            let conversion = map_tds_error(
                "operation failed",
                TdsError::TypeConversionError("conversion".into()),
                Vec::new(),
            );
            assert!(conversion.is_instance_of::<DataError>(py));

            let unsupported = map_tds_error(
                "operation failed",
                TdsError::UnimplementedFeature {
                    feature: "feature".into(),
                    context: "context".into(),
                },
                Vec::new(),
            );
            assert!(unsupported.is_instance_of::<NotSupportedError>(py));

            let protocol = map_tds_error(
                "operation failed",
                TdsError::ProtocolError("protocol".into()),
                Vec::new(),
            );
            assert!(protocol.is_instance_of::<InternalError>(py));

            let connection = map_tds_error(
                "operation failed",
                TdsError::ConnectionClosed("closed".into()),
                Vec::new(),
            );
            assert!(connection.is_instance_of::<OperationalError>(py));
        });
    }

    #[test]
    fn preserves_sql_server_diagnostics() {
        let diagnostics = SqlServerDiagnostics::new(
            vec![SqlErrorInfo {
                message: "test failure".to_string(),
                state: 2,
                class: 16,
                number: 50_001,
                server_name: Some("server".to_string()),
                proc_name: Some("procedure".to_string()),
                line_number: Some(7),
            }],
            vec![SqlInfoMessage {
                message: "existing info".to_string(),
                state: 1,
                class: 0,
                number: 0,
                server_name: Some("server".to_string()),
                proc_name: None,
                line_number: Some(5),
            }],
        );
        let error = map_tds_error(
            "PyAsyncCursor.nextset failed while advancing results",
            TdsError::SqlServerError { diagnostics },
            vec![SqlInfoMessage {
                message: "statement info".to_string(),
                state: 1,
                class: 10,
                number: 50_000,
                server_name: Some("server".to_string()),
                proc_name: Some("procedure".to_string()),
                line_number: Some(6),
            }],
        );

        Python::attach(|py| {
            assert!(error.is_instance_of::<DatabaseError>(py));
            let errors = error
                .value(py)
                .getattr("sql_errors")
                .unwrap()
                .extract::<Vec<Py<PyAny>>>()
                .unwrap();
            assert_eq!(errors.len(), 1);
            assert_eq!(
                errors[0]
                    .bind(py)
                    .get_item("number")
                    .unwrap()
                    .extract::<u32>()
                    .unwrap(),
                50_001
            );
            let info_messages = error
                .value(py)
                .getattr("info_messages")
                .unwrap()
                .extract::<Vec<Py<PyAny>>>()
                .unwrap();
            assert_eq!(info_messages.len(), 2);
            assert_eq!(
                info_messages[0]
                    .bind(py)
                    .get_item("message")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "existing info"
            );
            assert_eq!(
                info_messages[1]
                    .bind(py)
                    .get_item("message")
                    .unwrap()
                    .extract::<String>()
                    .unwrap(),
                "statement info"
            );
            assert_eq!(
                info_messages[1]
                    .bind(py)
                    .get_item("number")
                    .unwrap()
                    .extract::<u32>()
                    .unwrap(),
                50_000
            );
        });
    }
}
