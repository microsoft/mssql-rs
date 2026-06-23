// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use pyo3::prelude::*;
use std::sync::OnceLock;

use mssql_tds::connection::client_context::DriverVersion;

mod bulkcopy;
mod connection;
mod cursor;
mod python_entra_token_factory;
mod python_logger_adapter;
mod row_writer;
mod tracing_init;
mod types;
mod utils;

pub use python_logger_adapter::{init_tracing_bridge, scoped_tracing_bridge};

/// Module-level driver version, set once by the host Python package.
/// Falls back to mssql-tds crate version if never set.
pub(crate) static DRIVER_VERSION: OnceLock<DriverVersion> = OnceLock::new();

/// Unparsed raw module-level driver version string for User-Agent telemetry.
pub(crate) static RAW_DRIVER_VERSION: OnceLock<String> = OnceLock::new();

/// Module-level runtime details, set once by the host Python package.
pub(crate) static RUNTIME_DETAILS: OnceLock<String> = OnceLock::new();

/// Returns the driver version, falling back to the mssql-tds crate version.
pub(crate) fn get_driver_version() -> DriverVersion {
    DRIVER_VERSION
        .get()
        .copied()
        .unwrap_or_else(DriverVersion::from_cargo_version)
}

/// Returns the unparsed driver version string if set.
pub(crate) fn get_raw_driver_version() -> Option<String> {
    RAW_DRIVER_VERSION.get().cloned()
}

/// Python module for Core TDS connectivity
#[pymodule(name = "mssql_py_core")]
fn mssql_py_core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // Initialize tracing on module load (via MSSQL_TDS_TRACE env var)
    tracing_init::init_tracing();

    // Statically capture the Python version once during module initialization
    let py_version = pyo3::Python::version_str();
    let _ = RUNTIME_DETAILS.set(format!("Python {}", py_version));

    // Statically capture the mssql_python driver version during module initialization
    if let Ok(ver_str) = m
        .py()
        .import("mssql_python")
        .and_then(|module| module.getattr("__version__"))
        .and_then(|v| v.extract::<String>())
    {
        let parts: Vec<&str> = ver_str.split('.').collect();
        if parts.len() >= 3 {
            // Helper to solidly extract only leading digits from parts like "3rc1" or "12dev"
            let parse_part = |s: &str| {
                let digits_only: String = s.chars().take_while(|c| c.is_ascii_digit()).collect();
                digits_only
            };

            #[allow(clippy::collapsible_if)]
            if let (Ok(major), Ok(minor), Ok(build)) = (
                parse_part(parts[0]).parse::<u8>(),
                parse_part(parts[1]).parse::<u8>(),
                parse_part(parts[2]).parse::<u16>(),
            ) {
                let _ = DRIVER_VERSION.set(DriverVersion::new(major, minor, build));
            }
        }

        let _ = RAW_DRIVER_VERSION.set(ver_str);
    }

    m.add_class::<connection::PyCoreConnection>()?;
    m.add_class::<cursor::PyCoreCursor>()?;

    // Test-only hook to drive PythonEntraIdTokenFactory::create_token from
    // Python tests. Underscore-prefixed to mark as internal/test-only.
    m.add_function(wrap_pyfunction!(
        python_entra_token_factory::invoke_entra_id_token_factory,
        m
    )?)?;
    Ok(())
}
