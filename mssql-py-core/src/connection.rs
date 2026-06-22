// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use std::{path::PathBuf, sync::Arc};
use tokio::runtime::Runtime;
use tokio::sync::Mutex;

use crate::python_logger_adapter::scoped_tracing_bridge;
use mssql_tds::{
    connection::client_context::{ClientContext, IPAddressPreference},
    connection::odbc_authentication_transformer::transform_auth,
    connection::odbc_authentication_validator::validate_auth,
    connection::tds_client::TdsClient,
    connection_provider::tds_connection_provider::TdsConnectionProvider,
    core::{EncryptionOptions, EncryptionSetting},
    message::login_options::ApplicationIntent,
};

const DEFAULT_LIBRARY_NAME: &str = "MS-PYTHON";

/// Python Connection class for Core TDS backend
#[pyclass]
pub struct PyCoreConnection {
    #[allow(dead_code)] // Used for async operations in cursor execute
    runtime: Runtime,
    tds_client: Option<Arc<Mutex<TdsClient>>>,
    is_closed: bool,
}

#[pymethods]
impl PyCoreConnection {
    #[new]
    #[pyo3(signature = (client_context_dict, python_logger=None))]
    fn new(
        client_context_dict: &Bound<'_, PyDict>,
        python_logger: Option<&Bound<'_, PyAny>>,
    ) -> PyResult<Self> {
        // Set up tracing bridge if logger provided
        let _guard = python_logger
            .map(|logger| scoped_tracing_bridge(Arc::new(logger.clone().unbind()), file!()));

        tracing::info!("Creating new PyCoreConnection");
        let runtime = Runtime::new().map_err(|e| {
            tracing::error!("Failed to create Tokio runtime: {}", e);
            PyRuntimeError::new_err(format!("Failed to create runtime: {e}"))
        })?;

        // Convert PyDict to ClientContext
        tracing::debug!("Converting Python dict to ClientContext");
        let client_context = Self::dict_to_client_context(client_context_dict)?;

        // Log encryption/TLS details for diagnosing handshake failures
        tracing::info!(
            "Encryption options: mode={:?}, trust_server_certificate={}, host_name_in_cert={:?}, server_certificate={:?}",
            client_context.encryption_options.mode,
            client_context.encryption_options.trust_server_certificate,
            client_context.encryption_options.host_name_in_cert,
            client_context.encryption_options.server_certificate,
        );

        // Connect using TdsConnectionProvider
        tracing::info!(
            "Attempting connection to datasource: {}",
            client_context.data_source
        );
        let datasource = client_context.data_source.clone();
        let provider = TdsConnectionProvider {};
        let tds_client = runtime.block_on(async {
            provider
                .create_client(client_context, &datasource, None)
                .await
        });

        match tds_client {
            Ok(client) => {
                tracing::info!("Successfully connected to SQL Server");
                Ok(PyCoreConnection {
                    runtime,
                    tds_client: Some(Arc::new(Mutex::new(client))),
                    is_closed: false,
                })
            }
            Err(e) => {
                tracing::error!("Failed to connect to SQL Server: {}", e);
                Err(PyRuntimeError::new_err(format!(
                    "Failed to connect to SQL Server: {e}"
                )))
            }
        }
    }

    fn close(&mut self) -> PyResult<()> {
        if !self.is_closed {
            // Send TDS close to the server and shut down the TCP connection
            if let Some(client) = self.tds_client.take() {
                self.runtime.block_on(async {
                    let mut guard = client.lock().await;
                    if let Err(e) = guard.close_connection().await {
                        tracing::warn!("Error closing connection: {}", e);
                    }
                });
            }
            self.is_closed = true;
        }
        Ok(())
    }

    fn cursor(&self) -> PyResult<crate::cursor::PyCoreCursor> {
        if self.is_closed {
            return Err(PyRuntimeError::new_err("Connection is closed"));
        }

        if let Some(client) = &self.tds_client {
            // Pass runtime handle to cursor so it can use the same runtime
            let handle = self.runtime.handle().clone();
            Ok(crate::cursor::PyCoreCursor::new(client.clone(), handle))
        } else {
            Err(PyRuntimeError::new_err("No active connection"))
        }
    }

    fn commit(&mut self) -> PyResult<()> {
        if self.is_closed {
            return Err(PyRuntimeError::new_err("Connection is closed"));
        }

        // TODO: Implement transaction commit
        Ok(())
    }

    fn rollback(&mut self) -> PyResult<()> {
        if self.is_closed {
            return Err(PyRuntimeError::new_err("Connection is closed"));
        }

        // TODO: Implement transaction rollback
        Ok(())
    }

    fn is_connected(&self) -> PyResult<bool> {
        Ok(!self.is_closed && self.tds_client.is_some())
    }

    fn __repr__(&self) -> String {
        if self.is_closed {
            "DdbcConnection(closed)".to_string()
        } else {
            "DdbcConnection(connected)".to_string()
        }
    }
}

impl PyCoreConnection {
    /// Convert Python dict (ClientContext fields) to Rust ClientContext
    fn dict_to_client_context(dict: &Bound<'_, PyDict>) -> PyResult<ClientContext> {
        tracing::debug!("Extracting connection parameters from Python dict");
        // Extract required fields with defaults
        let server = dict
            .get_item("server")?
            .and_then(|v| v.extract::<String>().ok())
            .unwrap_or_else(|| "localhost".to_string());

        tracing::debug!("Server: {}", server);
        // Keep the server string as datasource for create_client
        let datasource = server.clone();

        let user_name = dict
            .get_item("user_name")?
            .and_then(|v| v.extract::<String>().ok())
            .unwrap_or_default();

        let password = dict
            .get_item("password")?
            .and_then(|v| v.extract::<String>().ok())
            .unwrap_or_default();

        let database = dict
            .get_item("database")?
            .and_then(|v| v.extract::<String>().ok())
            .unwrap_or_default();

        let application_name = dict
            .get_item("application_name")?
            .and_then(|v| v.extract::<String>().ok())
            .unwrap_or_else(|| "mssql-python".to_string());

        let connect_timeout = dict
            .get_item("connect_timeout")?
            .and_then(|v| v.extract::<u32>().ok())
            .unwrap_or(15);

        // Extract packet_size and validate it's within acceptable range
        let packet_size = dict
            .get_item("packet_size")?
            .and_then(|v| {
                // Accept both i32 (from Python int) and i64, then validate and convert to u16
                v.extract::<i64>()
                    .ok()
                    .or_else(|| v.extract::<i32>().ok().map(|x| x as i64))
                    .and_then(|size| {
                        if (512..=32768).contains(&size) {
                            Some(size as u16)
                        } else {
                            None
                        }
                    })
            })
            .unwrap_or(4096);

        let mars_enabled = dict
            .get_item("mars_enabled")?
            .and_then(|v| v.extract::<bool>().ok())
            .unwrap_or(false);

        let multi_subnet_failover =
            Self::extract_yes_no_bool(dict, "multi_subnet_failover", "MultiSubnetFailover")?;

        let trust_server_certificate =
            Self::extract_yes_no_bool(dict, "trust_server_certificate", "TrustServerCertificate")?;

        // Extract server_spn for Kerberos authentication
        let server_spn = dict
            .get_item("server_spn")?
            .and_then(|v| v.extract::<String>().ok());

        // HostnameInCertificate - used to specify the expected hostname in the server certificate
        // when it differs from the server name in the connection string
        let host_name_in_cert = dict
            .get_item("host_name_in_certificate")?
            .and_then(|v| v.extract::<String>().ok());

        // Parse encryption setting (case-insensitive)
        // Default matches ODBC Driver 18 secure-by-default: Encrypt=Yes (Mandatory)
        let encryption_str = dict
            .get_item("encryption")?
            .and_then(|v| v.extract::<String>().ok())
            .unwrap_or_else(|| "Mandatory".to_string());

        let encryption_mode = match encryption_str.to_ascii_lowercase().as_str() {
            "yes" | "true" | "mandatory" | "required" => EncryptionSetting::Required,
            "no" | "false" | "optional" | "disabled" => EncryptionSetting::PreferOff,
            "strict" => EncryptionSetting::Strict,
            other => {
                return Err(PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!(
                    "Invalid Encrypt value '{}'. Expected: Yes, No, True, False, Optional, Mandatory, Strict",
                    other
                )));
            }
        };

        // ServerCertificate - path to the server certificate file for validation
        let server_certificate = dict
            .get_item("server_certificate")?
            .and_then(|v| v.extract::<PathBuf>().ok());

        let encryption_options = EncryptionOptions {
            mode: encryption_mode,
            trust_server_certificate,
            host_name_in_cert,
            server_certificate,
        };

        // Parse application intent (case-insensitive)
        let application_intent_str = dict
            .get_item("application_intent")?
            .and_then(|v| v.extract::<String>().ok())
            .unwrap_or_else(|| "ReadWrite".to_string());

        let application_intent = match application_intent_str.to_ascii_lowercase().as_str() {
            "readonly" => ApplicationIntent::ReadOnly,
            "readwrite" => ApplicationIntent::ReadWrite,
            other => {
                return Err(PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!(
                    "Invalid ApplicationIntent value '{}'. Expected: ReadOnly, ReadWrite",
                    other
                )));
            }
        };

        let workstation_id = dict
            .get_item("workstation_id")?
            .and_then(|v| v.extract::<String>().ok())
            .unwrap_or_else(|| {
                hostname::get()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_string()
            });

        let language = dict
            .get_item("language")?
            .and_then(|v| v.extract::<String>().ok())
            .unwrap_or_else(|| "us_english".to_string());

        // Connection retry settings
        // Defaults: 1 retry attempt, 10 seconds between retries per SQL Server defaults
        let connect_retry_count = dict
            .get_item("connect_retry_count")?
            .and_then(|v| v.extract::<u32>().ok())
            .unwrap_or(1);

        let connect_retry_interval = dict
            .get_item("connect_retry_interval")?
            .and_then(|v| v.extract::<u32>().ok())
            .unwrap_or(10);

        // IpAddressPreference - controls IPv4 vs IPv6 preference for DNS resolution
        // Values: "IPv4First", "IPv6First", "UsePlatformDefault" (default)
        // Case-insensitive comparison
        let ipaddress_preference_str = dict
            .get_item("ip_address_preference")?
            .and_then(|v| v.extract::<String>().ok())
            .unwrap_or_else(|| "UsePlatformDefault".to_string());

        let ipaddress_preference = match ipaddress_preference_str.to_ascii_lowercase().as_str() {
            "ipv4first" => IPAddressPreference::IPv4First,
            "ipv6first" => IPAddressPreference::IPv6First,
            "useplatformdefault" => IPAddressPreference::UsePlatformDefault,
            other => {
                return Err(PyErr::new::<pyo3::exceptions::PyRuntimeError, _>(format!(
                    "Invalid IPAddressPreference value '{}'. Expected: IPv4First, IPv6First, UsePlatformDefault",
                    other
                )));
            }
        };

        // TCP Keep-alive settings (milliseconds)
        // Defaults: 30000ms (30s) for keep_alive, 1000ms (1s) for interval per SQL Server defaults
        // Named to match ODBC Driver's "KeepAlive" and "KeepAliveInterval" parameters
        let keep_alive_in_ms = dict
            .get_item("keep_alive")?
            .and_then(|v| v.extract::<u32>().ok())
            .unwrap_or(30_000);

        let keep_alive_interval_in_ms = dict
            .get_item("keep_alive_interval")?
            .and_then(|v| v.extract::<u32>().ok())
            .unwrap_or(1_000);

        // Extract raw JWT access token (if provided by Python at bulk copy time).
        // This is a fresh token acquired by mssql-python's Azure Identity SDK,
        // NOT the ODBC struct format — just the plain JWT string.
        let access_token: Option<String> = dict
            .get_item("access_token")?
            .and_then(|v| v.extract::<String>().ok());

        // Authentication keyword (e.g., "ActiveDirectoryPassword", "SqlPassword")
        let authentication: Option<String> = dict
            .get_item("authentication")?
            .and_then(|v| v.extract::<String>().ok());

        // Trusted_Connection (Integrated Security / SSPI)
        // ODBC only accepts "Yes"/"No" — reject "true"/"false" and other values.
        let trusted_connection: Option<bool> = match dict
            .get_item("trusted_connection")?
            .and_then(|v| v.extract::<String>().ok())
        {
            Some(val) if val.eq_ignore_ascii_case("yes") => Some(true),
            Some(val) if val.eq_ignore_ascii_case("no") => Some(false),
            Some(val) => {
                return Err(PyRuntimeError::new_err(format!(
                    "Invalid Trusted_Connection value: '{val}'. Only 'Yes' or 'No' are accepted."
                )));
            }
            None => None,
        };

        // Validate auth inputs (ODBC-parity conflict checks)
        validate_auth(
            authentication.as_deref(),
            trusted_connection,
            &user_name,
            &password,
            access_token.as_deref(),
        )
        .map_err(|e| PyRuntimeError::new_err(e.to_string()))?;

        // Transform validated inputs into final auth method + cleaned credentials
        let transformed = transform_auth(
            authentication.as_deref(),
            trusted_connection,
            &user_name,
            &password,
            access_token.as_deref(),
        );

        // Create ClientContext with the data source (transport_context will be set by parse_datasource)
        tracing::debug!(
            "Creating ClientContext - database: {:?}, app: {}, timeout: {}s, packet_size: {}, encryption: {:?}",
            if database.is_empty() {
                None
            } else {
                Some(&database)
            },
            application_name,
            connect_timeout,
            packet_size,
            encryption_mode
        );
        let mut context = ClientContext::with_data_source(&datasource);
        context.user_name = transformed.user_name;
        context.password = transformed.password;
        context.database = database;
        context.application_name = application_name;
        context.connect_timeout = connect_timeout;
        context.connect_retry_count = connect_retry_count;
        context.connect_retry_interval = connect_retry_interval;
        context.packet_size = packet_size;
        context.mars_enabled = mars_enabled;
        context.multi_subnet_failover = multi_subnet_failover;
        context.encryption_options = encryption_options;
        context.application_intent = application_intent;
        context.workstation_id = workstation_id;
        context.language = language;
        context.server_spn = server_spn;
        context.ipaddress_preference = ipaddress_preference;
        context.keep_alive_in_ms = keep_alive_in_ms;
        context.keep_alive_interval_in_ms = keep_alive_interval_in_ms;
        // Block authentication methods not yet supported through py-core.
        // ActiveDirectoryIntegrated requires Kerberos ticket forwarding via
        // Entra ID which has no token-provider wired up yet.
        // ActiveDirectoryPassword requires an Entra ID token provider callback
        // that py-core does not register.
        match &transformed.method {
            mssql_tds::connection::client_context::TdsAuthenticationMethod::ActiveDirectoryIntegrated => {
                return Err(PyRuntimeError::new_err(
                    "Authentication method 'ActiveDirectoryIntegrated' is not currently supported by mssql-py-core."
                ));
            }
            mssql_tds::connection::client_context::TdsAuthenticationMethod::ActiveDirectoryPassword => {
                return Err(PyRuntimeError::new_err(
                    "Authentication method 'ActiveDirectoryPassword' is not currently supported by mssql-py-core."
                ));
            }
            _ => {}
        }

        context.tds_authentication_method = transformed.method;
        context.access_token = transformed.access_token;

        // Optional Entra ID token factory provided by Python (e.g. mssql-python
        // wires this up for ActiveDirectoryServicePrincipal to acquire a JWT
        // via azure-identity during the FedAuth handshake). The factory is
        // registered under the resolved auth method; mssql-tds dispatches to
        // it via `ClientContext::entra_id_token_factory()`.
        if let Some(factory_obj) = dict.get_item("entra_id_token_factory")?
            && !factory_obj.is_none()
        {
            if !factory_obj.is_callable() {
                return Err(pyo3::exceptions::PyTypeError::new_err(
                    "entra_id_token_factory must be a callable (e.g., a function) that returns an access token",
                ));
            }

            let factory = crate::python_entra_token_factory::PythonEntraIdTokenFactory::new(
                factory_obj.unbind(),
            );
            context
                .auth_method_map
                .insert(context.tds_authentication_method.clone(), Box::new(factory));

            // For factory-based AD methods (ServicePrincipal, Password) the
            // login-packet user_name/password fields are not serialized on the
            // wire: the factory captures the credentials itself and delivers a
            // JWT during FedAuth. Drop them from ClientContext so the
            // client_secret does not outlive the registration any longer than
            // necessary.
            if matches!(
                context.tds_authentication_method,
                mssql_tds::connection::client_context::TdsAuthenticationMethod::ActiveDirectoryServicePrincipal
                    | mssql_tds::connection::client_context::TdsAuthenticationMethod::ActiveDirectoryPassword
            ) {
                context.user_name = String::new();
                context.password = String::new();
            }
        }

        // Set library_name to "mssql-python" for Python driver
        context.library_name = "mssql-python".to_string();

        // Exclusively override the driver name for the User-Agent payload
        context
            .user_agent
            .set_library_name(DEFAULT_LIBRARY_NAME.to_string());

        if let Some(raw_version) = crate::get_raw_driver_version() {
            context.user_agent.set_driver_version(raw_version);
        }

        if let Some(global_details) = crate::RUNTIME_DETAILS.get() {
            context.user_agent.set_runtime(global_details.clone());
        }

        // Fetch driver version natively using module-level configuration or Cargo fallback
        context.driver_version = crate::get_driver_version();

        Ok(context)
    }

    /// Extract a Yes/No string (ODBC-style) or Python bool, defaulting to `false`.
    /// This is to ensure that ODBC parameters like MultiSubnetFailover and TrustServerCertificate only accept valid values, while still allowing Pythonic bools for convenience.
    fn extract_yes_no_bool(
        dict: &Bound<'_, PyDict>,
        key: &str,
        display_name: &str,
    ) -> PyResult<bool> {
        match dict.get_item(key)? {
            Some(v) if v.extract::<String>().is_ok() => {
                let s = v.extract::<String>()?;
                match s.to_ascii_lowercase().as_str() {
                    "yes" => Ok(true),
                    "no" => Ok(false),
                    _ => Err(PyErr::new::<PyRuntimeError, _>(format!(
                        "Invalid {display_name} value '{s}'. Only 'Yes' or 'No' are accepted."
                    ))),
                }
            }
            Some(v) => Ok(v.extract::<bool>().unwrap_or(false)),
            None => Ok(false),
        }
    }
}
