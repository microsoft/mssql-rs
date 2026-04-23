// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Python bindings for the mock TDS server
//!
//! This module exposes the mock TDS server to Python for testing purposes.
//! It allows Python tests to:
//! - Start a mock TDS server with FedAuth support
//! - Verify that access tokens were correctly received by the server

use mssql_mock_tds::MockTdsServer;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use std::path::Path;
use std::sync::Arc;
use tokio::runtime::Runtime;
use tokio::sync::{Mutex, oneshot};

/// Load test TLS identity from certificate files
/// Looks for certificates in mssql-tds/tests/test_certificates/ relative to the workspace
fn load_test_identity() -> Result<native_tls::Identity, Box<dyn std::error::Error>> {
    // Try multiple possible paths for the certificates
    let possible_paths = [
        "mssql-tds/tests/test_certificates", // Running from mssql-tds workspace root
        "tests/test_certificates",           // Running from mssql-tds crate
        "../mssql-tds/tests/test_certificates", // Running from sibling crate
    ];

    for base_path in &possible_paths {
        let cert_path = format!("{}/valid_cert.pem", base_path);
        let key_path = format!("{}/key.pem", base_path);

        if Path::new(&cert_path).exists() && Path::new(&key_path).exists() {
            let cert_pem = std::fs::read(&cert_path)?;
            let key_pem = std::fs::read(&key_path)?;
            return mssql_mock_tds::create_test_identity(&cert_pem, &key_pem);
        }

        // Also try .pfx file (for Windows compatibility)
        let pfx_path = format!("{}/identity.pfx", base_path);
        if Path::new(&pfx_path).exists() {
            return mssql_mock_tds::load_identity_from_file(&pfx_path, "");
        }
    }

    Err("Test certificates not found. Generate them using:\n\
         ./scripts/generate_mock_tds_server_certs.sh\n\
         \n\
         Expected files:\n\
         - mssql-tds/tests/test_certificates/valid_cert.pem\n\
         - mssql-tds/tests/test_certificates/key.pem"
        .into())
}

/// Information about a connection received by the mock server
#[pyclass]
#[derive(Clone)]
pub struct PyConnectionInfo {
    /// Client address as string
    #[pyo3(get)]
    pub addr: String,
    /// Access token received (as string, decoded from UTF-16LE)
    #[pyo3(get)]
    pub access_token: Option<String>,
    /// Whether the client authenticated successfully
    #[pyo3(get)]
    pub authenticated: bool,
    /// User Agent sent by the client
    #[pyo3(get)]
    pub user_agent: Option<String>,
}

#[pymethods]
impl PyConnectionInfo {
    fn __repr__(&self) -> String {
        format!(
            "ConnectionInfo(addr='{}', authenticated={}, user_agent='{:?}', has_token={})",
            self.addr,
            self.authenticated,
            self.user_agent,
            self.access_token.is_some()
        )
    }
}

/// Python wrapper for the Mock TDS Server
#[pyclass]
pub struct PyMockTdsServer {
    /// Tokio runtime for async operations
    runtime: Arc<Runtime>,
    /// The actual mock server (wrapped in Arc<Mutex> for thread safety)
    server: Option<Arc<MockTdsServer>>,
    /// Shutdown channel sender
    shutdown_tx: Option<oneshot::Sender<()>>,
    /// Server handle
    server_handle: Option<std::thread::JoinHandle<()>>,
    /// Local address the server is bound to
    local_addr: String,
    /// Port the server is listening on
    port: u16,
    /// Connection store for accessing received tokens
    connection_store: Arc<Mutex<mssql_mock_tds::server::ConnectionStore>>,
}

#[pymethods]
impl PyMockTdsServer {
    /// Create a new mock TDS server.
    ///
    /// The server always supports both FedAuth (access token) and username/password authentication.
    ///
    /// Args:
    ///     port: Port to listen on (0 for automatic port selection)
    ///     tls: Enable TLS encryption (requires certificates in mssql-tds/tests/test_certificates/)
    ///
    /// Returns:
    ///     PyMockTdsServer instance
    #[new]
    #[pyo3(signature = (port=0, tls=false))]
    pub fn new(port: u16, tls: bool) -> PyResult<Self> {
        let runtime = Runtime::new().map_err(|e| {
            PyRuntimeError::new_err(format!("Failed to create Tokio runtime: {}", e))
        })?;

        let addr = format!("127.0.0.1:{}", port);

        // Create the server - FedAuth and user/pass are always supported
        let server = runtime
            .block_on(async {
                if tls {
                    // Load TLS identity from test certificates
                    let identity = load_test_identity().map_err(|e| {
                        std::io::Error::other(format!("Failed to load TLS identity: {}", e))
                    })?;
                    mssql_mock_tds::MockTdsServer::new_with_tls(&addr, Some(identity)).await
                } else {
                    MockTdsServer::new(&addr).await
                }
            })
            .map_err(|e| PyRuntimeError::new_err(format!("Failed to create mock server: {}", e)))?;

        let local_addr = server.local_addr();
        let actual_port = local_addr.port();
        let connection_store = server.connection_store();

        Ok(Self {
            runtime: Arc::new(runtime),
            server: Some(Arc::new(server)),
            shutdown_tx: None,
            server_handle: None,
            local_addr: local_addr.to_string(),
            port: actual_port,
            connection_store,
        })
    }

    /// Start the server in the background
    ///
    /// This starts the server in a background thread. Use stop() to shut it down.
    pub fn start(&mut self) -> PyResult<()> {
        if self.server_handle.is_some() {
            return Err(PyRuntimeError::new_err("Server is already running"));
        }

        let server = self
            .server
            .take()
            .ok_or_else(|| PyRuntimeError::new_err("Server has already been started or stopped"))?;

        let (shutdown_tx, shutdown_rx) = oneshot::channel();
        self.shutdown_tx = Some(shutdown_tx);

        let runtime = self.runtime.clone();

        // Run the server in a separate thread
        let handle = std::thread::spawn(move || {
            runtime.block_on(async move {
                if let Err(e) = Arc::try_unwrap(server)
                    .unwrap_or_else(|_| panic!("Server has multiple references"))
                    .run_with_shutdown(shutdown_rx)
                    .await
                {
                    eprintln!("Server error: {}", e);
                }
            });
        });

        self.server_handle = Some(handle);
        Ok(())
    }

    /// Stop the server
    pub fn stop(&mut self) -> PyResult<()> {
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }

        if let Some(handle) = self.server_handle.take() {
            handle
                .join()
                .map_err(|_| PyRuntimeError::new_err("Failed to join server thread"))?;
        }

        Ok(())
    }

    /// Get the server address (host:port format)
    #[getter]
    pub fn address(&self) -> String {
        self.local_addr.clone()
    }

    /// Get the server address in SQL Server format (host,port)
    #[getter]
    pub fn sql_address(&self) -> String {
        format!("127.0.0.1,{}", self.port)
    }

    /// Get the port the server is listening on
    #[getter]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Get the number of connections received
    pub fn connection_count(&self) -> PyResult<usize> {
        let runtime = &self.runtime;
        let store = self.connection_store.clone();

        Ok(runtime.block_on(async { store.lock().await.count() }))
    }

    /// Get all received connection info
    pub fn get_connections(&self) -> PyResult<Vec<PyConnectionInfo>> {
        let runtime = &self.runtime;
        let store = self.connection_store.clone();

        Ok(runtime.block_on(async {
            let store = store.lock().await;
            store
                .all()
                .values()
                .map(|info| PyConnectionInfo {
                    addr: info.addr.to_string(),
                    access_token: info.received_token_as_string(),
                    authenticated: info.authenticated,
                    user_agent: info.user_agent.clone(),
                })
                .collect()
        }))
    }

    /// Get the access token from the most recent connection
    ///
    /// Returns None if no connections have been made or no token was received.
    pub fn get_last_access_token(&self) -> PyResult<Option<String>> {
        let runtime = &self.runtime;
        let store = self.connection_store.clone();

        Ok(runtime.block_on(async {
            let store = store.lock().await;
            store
                .all()
                .values()
                .last()
                .and_then(|info| info.received_token_as_string())
        }))
    }

    /// Clear all stored connection info
    pub fn clear_connections(&self) -> PyResult<()> {
        let runtime = &self.runtime;
        let store = self.connection_store.clone();

        runtime.block_on(async {
            store.lock().await.clear();
        });

        Ok(())
    }

    /// Check if any connection received the specified access token
    ///
    /// Args:
    ///     token: The access token to look for
    ///
    /// Returns:
    ///     True if any connection received this exact token
    pub fn has_received_token(&self, token: &str) -> PyResult<bool> {
        let runtime = &self.runtime;
        let store = self.connection_store.clone();

        Ok(runtime.block_on(async {
            let store = store.lock().await;
            store
                .all()
                .values()
                .any(|info| info.received_token_as_string().as_deref() == Some(token))
        }))
    }

    fn __enter__(slf: Py<Self>, py: Python<'_>) -> PyResult<Py<Self>> {
        slf.borrow_mut(py).start()?;
        Ok(slf)
    }

    fn __exit__(
        &mut self,
        _exc_type: Option<&Bound<'_, PyAny>>,
        _exc_val: Option<&Bound<'_, PyAny>>,
        _exc_tb: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<bool> {
        self.stop()?;
        Ok(false)
    }

    fn __repr__(&self) -> String {
        format!(
            "PyMockTdsServer(address='{}', port={})",
            self.local_addr, self.port
        )
    }
}

/// Python module for mock TDS server bindings
#[pymodule]
fn mssql_mock_tds_py(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyMockTdsServer>()?;
    m.add_class::<PyConnectionInfo>()?;
    Ok(())
}
