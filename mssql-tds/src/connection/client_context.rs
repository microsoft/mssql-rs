// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use async_trait::async_trait;

use crate::connection::datasource_parser::{ParsedDataSource, ProtocolType};
use crate::core::{EncryptionOptions, EncryptionSetting, TdsResult};
use crate::error::Error;
use crate::message::login_options::{ApplicationIntent, TdsVersion};
use crate::security::{IntegratedAuthConfig, is_loopback_address};
use hostname;

/// Controls DNS resolution order when connecting to a server.
#[derive(PartialEq, Copy, Clone)]
pub enum IPAddressPreference {
    /// Resolve and attempt IPv4 addresses before IPv6.
    IPv4First = 0,
    /// Resolve and attempt IPv6 addresses before IPv4.
    IPv6First = 1,
    /// Use the platform's default resolution order.
    UsePlatformDefault = 2,
}

/// Represents a driver version with major, minor, and build components.
/// Used to populate `client_prog_ver` in the TDS Login7 packet.
///
/// Encoding: `[major (8 bits)][minor (8 bits)][build (16 bits)]`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DriverVersion {
    /// Major version number.
    pub major: u8,
    /// Minor version number.
    pub minor: u8,
    /// Build number.
    pub build: u16,
}

impl DriverVersion {
    /// Creates a new DriverVersion.
    pub fn new(major: u8, minor: u8, build: u16) -> Self {
        Self {
            major,
            minor,
            build,
        }
    }

    /// Creates a DriverVersion from the crate's Cargo.toml version at compile time.
    /// Parses the `CARGO_PKG_VERSION` environment variable (e.g., "0.1.0").
    pub fn from_cargo_version() -> Self {
        let parts: Vec<&str> = env!("CARGO_PKG_VERSION").split('.').collect();
        Self {
            major: parts.first().and_then(|s| s.parse().ok()).unwrap_or(0),
            minor: parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0),
            build: parts.get(2).and_then(|s| s.parse().ok()).unwrap_or(0),
        }
    }

    /// Encodes the version into a 32-bit integer for the TDS login packet.
    /// Format: `[major][minor][build_high][build_low]`
    pub fn encode(&self) -> i32 {
        ((self.major as i32) << 24) | ((self.minor as i32) << 16) | (self.build as i32)
    }
}

impl std::fmt::Display for DriverVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.build)
    }
}

/// Specifies the Vector feature version support level.
#[derive(PartialEq, Copy, Clone, Debug)]
pub enum VectorVersion {
    /// Vector support is disabled
    Off,
    /// Support Vector feature version 1 (float32 dimension type)
    V1,
    /// Support Vector feature version 2 (float16 and float32 dimension types)
    V2,
}

/// Controls the Always Encrypted (column encryption) behavior for a connection.
///
/// When `Enabled`, the client negotiates the Column Encryption (TCE) feature during
/// login, transparently encrypts parameters targeting encrypted columns, and decrypts
/// encrypted result columns. When `Disabled` (the default), the feature is not
/// negotiated and the connection behaves as if Always Encrypted is unavailable.
#[derive(PartialEq, Eq, Copy, Clone, Debug, Default)]
pub enum ColumnEncryptionSetting {
    /// Always Encrypted is disabled. The TCE feature is not requested. (Default.)
    #[default]
    Disabled,
    /// Always Encrypted is enabled. The TCE feature is negotiated during login.
    Enabled,
}

/// Per-execution override of the connection's Always Encrypted behavior.
///
/// A command may override the connection-level [`ColumnEncryptionSetting`] for a
/// single execution. The default, [`UseConnectionSetting`](Self::UseConnectionSetting),
/// inherits the connection's behavior. The override only has effect when the
/// server acknowledged the Column Encryption feature during login (which only
/// happens when the connection requested [`ColumnEncryptionSetting::Enabled`]).
#[derive(PartialEq, Eq, Copy, Clone, Debug, Default)]
pub enum ExecutionColumnEncryptionSetting {
    /// Inherit the connection's [`ColumnEncryptionSetting`]. (Default.)
    #[default]
    UseConnectionSetting,
    /// Encrypt parameters targeting encrypted columns and decrypt encrypted
    /// result columns for this command.
    Enabled,
    /// Decrypt encrypted result columns but send parameters unencrypted. Useful
    /// when a command reads encrypted columns but its parameters do not target
    /// any encrypted column.
    ResultSetOnly,
    /// Disable Always Encrypted for this command: parameters are sent
    /// unencrypted and result columns are not decrypted.
    Disabled,
}

/// Provides a trait for creating Entra ID tokens.
#[async_trait]
pub trait EntraIdTokenFactory: Send + Sync {
    /// Creates an access token for the given SPN, STS URL, and auth method.
    async fn create_token(
        &self,
        spn: String,
        sts_url: String,
        auth_method: TdsAuthenticationMethod,
    ) -> TdsResult<Vec<u8>>;
}
/// Object-safe extension of [`EntraIdTokenFactory`] that supports cloning.
pub trait CloneableEntraIdTokenFactory: EntraIdTokenFactory {
    /// Returns a boxed clone of this factory.
    fn clone_box(&self) -> Box<dyn CloneableEntraIdTokenFactory>;
}

impl<T> CloneableEntraIdTokenFactory for T
where
    T: EntraIdTokenFactory + Clone + 'static,
{
    fn clone_box(&self) -> Box<dyn CloneableEntraIdTokenFactory> {
        Box::new(self.clone())
    }
}

/// Authentication method for the TDS connection.
#[derive(Clone, Hash, Eq, PartialEq, Debug)]
pub enum TdsAuthenticationMethod {
    /// SQL Server authentication with username and password.
    Password,
    /// Integrated authentication via SSPI (Windows) or GSSAPI (Linux/macOS).
    SSPI,
    /// Azure Active Directory password authentication.
    ActiveDirectoryPassword,
    /// Azure AD interactive (browser-based) authentication.
    ActiveDirectoryInteractive,
    /// Azure AD device code flow for headless environments.
    ActiveDirectoryDeviceCodeFlow,
    /// Azure AD service principal (client ID + secret/cert).
    ActiveDirectoryServicePrincipal,
    /// Azure AD managed identity (system or user-assigned).
    ActiveDirectoryManagedIdentity,
    /// Azure AD default credential chain.
    ActiveDirectoryDefault,
    /// Azure AD managed service identity (legacy alias for `ActiveDirectoryManagedIdentity`).
    ActiveDirectoryMSI,
    /// Azure AD workload identity for Kubernetes workloads.
    ActiveDirectoryWorkloadIdentity,
    /// Azure AD integrated authentication using current user's Kerberos ticket.
    ActiveDirectoryIntegrated,
    /// Pre-acquired access token (bearer JWT).
    AccessToken,
}

/// Trait for validating ClientContext before establishing a connection.
/// This trait can be implemented by users to provide custom validation logic.
pub trait ClientContextValidator {
    /// Validates the ClientContext.
    /// Returns Ok(()) if validation passes, or an Error if validation fails.
    fn validate(&self, context: &ClientContext) -> TdsResult<()>;
}

/// Default validator that implements standard validation rules.
pub struct DefaultClientContextValidator;

impl ClientContextValidator for DefaultClientContextValidator {
    fn validate(&self, context: &ClientContext) -> TdsResult<()> {
        // Validate packet_size is within acceptable range (512 - 32768)
        const MIN_PACKET_SIZE: u16 = 512;
        const MAX_PACKET_SIZE: u16 = 32768;

        if context.packet_size < MIN_PACKET_SIZE || context.packet_size > MAX_PACKET_SIZE {
            return Err(Error::UsageError(format!(
                "Invalid packet size: {}. Packet size must be between {} and {} bytes.",
                context.packet_size, MIN_PACKET_SIZE, MAX_PACKET_SIZE
            )));
        }

        Ok(())
    }
}

use std::collections::HashMap;

/// Connection configuration for a TDS session.
///
/// Contains credentials, encryption settings, timeouts, and protocol options.
/// Construct via [`ClientContext::with_data_source()`] and pass to
/// [`TdsConnectionProvider::create_client()`](crate::connection_provider::tds_connection_provider::TdsConnectionProvider::create_client).
pub struct ClientContext {
    /// Read-write or read-only application intent. Default: `ReadWrite`.
    pub application_intent: ApplicationIntent,
    /// Application name reported in the TDS login packet.
    pub application_name: String,
    /// Database file to attach during login (AttachDBFileName).
    pub attach_db_file: String,
    /// New password to set during login (password change flow).
    pub change_password: String,
    /// Number of reconnection attempts after an idle connection failure.
    pub connect_retry_count: u32,
    /// Interval in seconds between connection retry attempts.
    /// Default: 10 seconds per SQL Server client defaults.
    /// Note: Not yet implemented internally - this field is reserved for future use.
    pub connect_retry_interval: u32,
    /// Connection timeout in seconds.
    pub connect_timeout: u32,
    /// Initial database catalog.
    pub database: String,
    /// The original data source string used to create this connection.
    /// This is a mandatory field - a connection cannot be established without it.
    /// Examples: "tcp:myserver,1433", "myserver\instance", "lpc:."
    pub data_source: String,
    /// TCP keep-alive idle time in milliseconds before first probe is sent.
    /// Default: 30000 (30 seconds) per SQL Server client defaults.
    /// Named to match ODBC Driver's "KeepAlive" connection string parameter.
    pub keep_alive_in_ms: u32,
    /// TCP keep-alive interval in milliseconds between subsequent probes.
    /// Default: 1000 (1 second) per SQL Server client defaults.
    pub keep_alive_interval_in_ms: u32,
    /// Named instance name. Default: `"MSSQLServer"`.
    pub database_instance: String,
    /// Whether to auto-enlist in the caller's distributed transaction.
    pub enlist: bool,
    /// TLS/encryption settings for the connection.
    pub encryption_options: EncryptionOptions,
    /// Failover partner server for database mirroring.
    pub failover_partner: String,
    /// DNS resolution order preference.
    pub ipaddress_preference: IPAddressPreference,
    /// Initial language for the session.
    pub language: String,
    /// Client library name sent in the login packet.
    pub library_name: String,
    /// Driver version used to populate `client_prog_ver` in the TDS Login7 packet.
    /// Defaults to the crate version from Cargo.toml.
    pub driver_version: DriverVersion,
    /// Token factories keyed by authentication method for Azure AD flows.
    pub auth_method_map: HashMap<TdsAuthenticationMethod, Box<dyn CloneableEntraIdTokenFactory>>,
    /// Enable Multiple Active Result Sets.
    pub mars_enabled: bool,
    /// Enable multi-subnet failover for AlwaysOn availability groups.
    pub multi_subnet_failover: bool,
    /// Timeout in milliseconds for SQL Browser (SSRP) instance resolution.
    /// Defaults to 1000ms.
    pub ssrp_timeout_ms: u64,
    /// New password to set (separate from `change_password` flow).
    pub new_password: String,
    /// TDS packet size in bytes. Valid range: 512–32768.
    pub packet_size: u16,
    /// Password for SQL Server authentication.
    pub password: String,
    /// Reserved for connection pooling support.
    pub pooling: bool,
    /// Enable replication support in the login flags.
    pub replication: bool,
    /// Authentication method to use.
    pub tds_authentication_method: TdsAuthenticationMethod,
    /// Connect to a user instance of SQL Server Express.
    pub user_instance: bool,
    /// Login user name for SQL Server authentication.
    pub user_name: String,
    /// Workstation name sent in the login packet. Defaults to hostname.
    pub workstation_id: String,
    /// Base64 encoded JWT access token for Azure AD authentication.
    pub access_token: Option<String>,
    /// Server Principal Name (SPN) for integrated authentication.
    /// If not provided, the SPN will be automatically generated from the server address.
    /// Format: MSSQLSvc/<hostname>:<port> or MSSQLSvc/<hostname>:<instance>
    pub server_spn: Option<String>,
    pub(crate) transport_context: TransportContext,
    /// Protocol vector version for feature negotiation.
    pub vector_version: VectorVersion,
    /// Always Encrypted (column encryption) setting for the connection.
    /// Default: [`ColumnEncryptionSetting::Disabled`].
    pub column_encryption_setting: ColumnEncryptionSetting,
    /// Registry of column master key store providers used to unwrap column
    /// encryption keys for Always Encrypted. Empty by default.
    pub(crate) column_encryption_key_store_providers:
        std::sync::Arc<crate::security::keystore::ColumnEncryptionKeyStoreProviderRegistry>,
    /// Cache of decrypted column encryption keys, shared across the connection
    /// (and its session-recovery clones) so a CMK only unwraps a CEK once.
    pub(crate) cek_cache: std::sync::Arc<crate::security::keystore::CekCache>,
    /// UserAgent telemetry payload components.
    pub user_agent: UserAgent,
}

const DEFAULT_LIBRARY_NAME: &str = "MS-TDS";
const UNKNOWN_RUNTIME: &str = "Unknown";

/// A grouping of telemetry-specific fields.
#[derive(Clone, Debug)]
pub struct UserAgent {
    /// Custom library name for User-Agent payload (e.g., `MS-PYTHON`).
    pub library_name: String,
    /// Custom driver version string for User-Agent payload (e.g., `1.2.3rc1`).
    pub driver_version: String,
    /// Custom runtime details (e.g. `CPython 3.12.3`).
    pub runtime: String,
}

impl Default for UserAgent {
    fn default() -> Self {
        Self {
            library_name: DEFAULT_LIBRARY_NAME.to_string(),
            driver_version: env!("CARGO_PKG_VERSION").to_string(),
            runtime: UNKNOWN_RUNTIME.to_string(),
        }
    }
}

impl UserAgent {
    /// Sets the custom library name for the User-Agent payload.
    pub fn set_library_name(&mut self, name: String) {
        self.library_name = name;
    }

    /// Sets the custom driver version for the User-Agent payload.
    pub fn set_driver_version(&mut self, version: String) {
        self.driver_version = version;
    }

    /// Sets the custom runtime details for the User-Agent payload.
    pub fn set_runtime(&mut self, runtime: String) {
        self.runtime = runtime;
    }
}

impl ClientContext {
    /// Registers a column master key store provider used to unwrap column
    /// encryption keys for Always Encrypted.
    ///
    /// `name` is matched case-insensitively against the `key_store_name`
    /// carried in the result-set CEK table (for example
    /// `MSSQL_CERTIFICATE_STORE`). Register providers before creating the
    /// client so they are available for the first encrypted query or bulk copy.
    ///
    /// # Example
    /// ```ignore
    /// use std::sync::Arc;
    /// use mssql_tds::security::RsaKeyStoreProvider;
    ///
    /// let mut provider = RsaKeyStoreProvider::new();
    /// provider.add_key_from_pem("CurrentUser/My/<thumbprint>", pem_bytes)?;
    ///
    /// let mut context = ClientContext::with_data_source("tcp:localhost,1433");
    /// context.register_column_encryption_key_store_provider(
    ///     "MSSQL_CERTIFICATE_STORE",
    ///     Arc::new(provider),
    /// );
    /// ```
    pub fn register_column_encryption_key_store_provider(
        &mut self,
        name: impl AsRef<str>,
        provider: std::sync::Arc<dyn crate::security::ColumnEncryptionKeyStoreProvider>,
    ) {
        std::sync::Arc::make_mut(&mut self.column_encryption_key_store_providers)
            .register(name, provider);
    }

    /// Creates a new ClientContext with the specified data source.
    /// The data source is mandatory for establishing a connection.
    ///
    /// # Arguments
    /// * `data_source` - The data source string (e.g., "tcp:myserver,1433", "myserver\\instance")
    ///
    /// # Example
    /// ```
    /// let context = ClientContext::with_data_source("tcp:myserver,1433");
    /// ```
    pub fn with_data_source(data_source: &str) -> ClientContext {
        ClientContext {
            application_intent: ApplicationIntent::ReadWrite,
            application_name: "TDSX Rust Client".to_string(),
            attach_db_file: "".to_string(),
            change_password: "".to_string(),
            connect_retry_count: 1,
            connect_retry_interval: 10,
            connect_timeout: 15,
            database: "".to_string(),
            data_source: data_source.to_string(),
            keep_alive_in_ms: 30_000, // 30 seconds (SQL Server default)
            keep_alive_interval_in_ms: 1_000, // 1 second (SQL Server default)
            database_instance: "MSSQLServer".to_string(),
            enlist: false,
            encryption_options: EncryptionOptions::new(),
            failover_partner: "".to_string(),
            ipaddress_preference: IPAddressPreference::UsePlatformDefault,
            language: "us_english".to_string(),
            library_name: "MS-TDS".to_string(),
            driver_version: DriverVersion::from_cargo_version(),
            auth_method_map: HashMap::new(),
            mars_enabled: false,
            multi_subnet_failover: false,
            ssrp_timeout_ms: crate::ssrp::DEFAULT_SSRP_TIMEOUT_MS,
            new_password: "".to_string(),
            packet_size: 8000,
            password: "".to_string(),
            pooling: false,
            replication: false,
            server_spn: None,
            tds_authentication_method: TdsAuthenticationMethod::Password,
            user_instance: false,
            user_name: "".to_string(),
            workstation_id: ClientContext::default_workstation_id(hostname::get),
            access_token: None,
            transport_context: TransportContext::Tcp {
                host: "localhost".to_string(),
                port: 1433,
                instance_name: None,
            },
            // TODO: make V2 as default when full V2 support is added
            vector_version: VectorVersion::V1,
            column_encryption_setting: ColumnEncryptionSetting::Disabled,
            column_encryption_key_store_providers: std::sync::Arc::new(
                crate::security::keystore::ColumnEncryptionKeyStoreProviderRegistry::new(),
            ),
            cek_cache: std::sync::Arc::new(crate::security::keystore::CekCache::new()),
            user_agent: UserAgent::default(),
        }
    }

    /// Creates a new ClientContext with default values.
    /// Note: The data_source field will be empty and must be set before connecting,
    /// either directly or by calling parse_datasource().
    ///
    /// Consider using `with_data_source()` instead for clearer intent.
    #[deprecated(
        since = "0.2.0",
        note = "Use with_data_source() instead for clearer intent"
    )]
    pub fn new() -> ClientContext {
        ClientContext {
            application_intent: ApplicationIntent::ReadWrite,
            application_name: "TDSX Rust Client".to_string(),
            attach_db_file: "".to_string(),
            change_password: "".to_string(),
            connect_retry_count: 1,
            connect_retry_interval: 10,
            connect_timeout: 15,
            database: "".to_string(),
            data_source: "".to_string(),
            keep_alive_in_ms: 30_000, // 30 seconds (SQL Server default)
            keep_alive_interval_in_ms: 1_000, // 1 second (SQL Server default)
            database_instance: "MSSQLServer".to_string(),
            enlist: false,
            encryption_options: EncryptionOptions::new(),
            failover_partner: "".to_string(),
            ipaddress_preference: IPAddressPreference::UsePlatformDefault,
            language: "us_english".to_string(),
            library_name: "MS-TDS".to_string(),
            driver_version: DriverVersion::from_cargo_version(),
            auth_method_map: HashMap::new(),
            mars_enabled: false,
            multi_subnet_failover: false,
            ssrp_timeout_ms: crate::ssrp::DEFAULT_SSRP_TIMEOUT_MS,
            new_password: "".to_string(),
            packet_size: 8000,
            password: "".to_string(),
            pooling: false,
            replication: false,
            tds_authentication_method: TdsAuthenticationMethod::Password,
            user_instance: false,
            user_name: "".to_string(),
            workstation_id: ClientContext::default_workstation_id(hostname::get),
            server_spn: None,
            access_token: None,
            transport_context: TransportContext::Tcp {
                host: "localhost".to_string(),
                port: 1433,
                instance_name: None,
            },
            // TODO: make V2 as default when full V2 support is added
            vector_version: VectorVersion::V1,
            column_encryption_setting: ColumnEncryptionSetting::Disabled,
            column_encryption_key_store_providers: std::sync::Arc::new(
                crate::security::keystore::ColumnEncryptionKeyStoreProviderRegistry::new(),
            ),
            cek_cache: std::sync::Arc::new(crate::security::keystore::CekCache::new()),
            user_agent: UserAgent::default(),
        }
    }

    /// Returns `true` if SSPI integrated authentication is configured.
    pub fn integrated_security(&self) -> bool {
        matches!(
            self.tds_authentication_method,
            TdsAuthenticationMethod::SSPI
        )
    }

    pub(crate) fn tds_version(&self) -> TdsVersion {
        if matches!(self.encryption_options.mode, EncryptionSetting::Strict) {
            TdsVersion::V8_0
        } else {
            TdsVersion::V7_4
        }
    }

    /// Encodes the driver version into a 32-bit integer for the TDS login packet.
    pub fn encode_driver_version(&self) -> i32 {
        self.driver_version.encode()
    }

    fn clone_auth_method_map(
        &self,
    ) -> HashMap<TdsAuthenticationMethod, Box<dyn CloneableEntraIdTokenFactory>> {
        self.auth_method_map
            .iter()
            .map(|(key, value)| (key.clone(), value.clone_box()))
            .collect()
    }

    /// Creates an IntegratedAuthConfig from this ClientContext.
    ///
    /// This is used when setting up SSPI/GSSAPI authentication.
    pub fn integrated_auth_config(&self) -> IntegratedAuthConfig {
        let is_loopback = match &self.transport_context {
            TransportContext::Tcp { host, .. } => is_loopback_address(host),
            // For named pipes, extract server from pipe_name (\\server\pipe\...)
            TransportContext::NamedPipe { pipe_name } => {
                // Extract server from \\server\pipe\... format
                let server = pipe_name
                    .trim_start_matches("\\\\")
                    .split('\\')
                    .next()
                    .unwrap_or(".");
                is_loopback_address(server)
            }
            // Shared memory is always local
            TransportContext::SharedMemory { .. } => true,
            // LocalDB is always local
            #[cfg(windows)]
            TransportContext::LocalDB { .. } => true,
        };

        IntegratedAuthConfig {
            server_spn: self.server_spn.clone(),
            security_package: Default::default(),
            // Populated post-handshake in `send_login7_request` from the TLS
            // engine's channel binding token (see
            // `NetworkWriter::channel_binding_token`).
            channel_bindings: None,
            is_loopback,
        }
    }

    /// Validates the ClientContext using the default validator.
    /// This method can be called before opening a connection to ensure the context is valid.
    ///
    /// # Returns
    /// Ok(()) if validation passes, or an Error if validation fails.
    pub fn validate(&self) -> TdsResult<()> {
        DefaultClientContextValidator.validate(self)
    }

    /// Validates the ClientContext using a custom validator.
    /// This allows callers to provide their own validation logic.
    ///
    /// # Arguments
    /// * `validator` - A custom validator implementing ClientContextValidator trait
    ///
    /// # Returns
    /// Ok(()) if validation passes, or an Error if validation fails.
    pub fn validate_with<V: ClientContextValidator>(&self, validator: &V) -> TdsResult<()> {
        validator.validate(self)
    }

    /// Looks up the Entra ID token factory for the current authentication method.
    pub(crate) fn entra_id_token_factory(&self) -> TdsResult<&dyn CloneableEntraIdTokenFactory> {
        self.auth_method_map
            .get(&self.tds_authentication_method)
            .map(|f| f.as_ref())
            .ok_or_else(|| {
                Error::ConnectionError(format!(
                    "Authentication method '{:?}' is not supported. \
                     No token provider was registered for this method.",
                    self.tds_authentication_method
                ))
            })
    }
}

impl Default for ClientContext {
    #[allow(deprecated)]
    fn default() -> Self {
        Self::new()
    }
}

impl ClientContext {
    /// Generates a default workstation ID based on the hostname.
    /// If the hostname is longer than 128 characters, it truncates it to 128 characters.
    /// This function is used to ensure that the workstation ID does not exceed the maximum length
    /// allowed by the server.
    fn default_workstation_id<F>(get_hostname: F) -> String
    where
        F: Fn() -> Result<std::ffi::OsString, std::io::Error>,
    {
        let hostname = get_hostname()
            .unwrap_or_else(|_| "".into())
            .to_string_lossy()
            .to_string();
        if hostname.len() > 128 {
            hostname[..128].to_string()
        } else {
            hostname
        }
    }

    /// Parse a data source string and update the ClientContext with the parsed transport
    ///
    /// This method parses the data source string (e.g., "tcp:server,1433", "server\instance")
    /// and updates the transport_context field of the ClientContext.
    /// It also stores the original data source string for logging and diagnostics.
    ///
    /// # Arguments
    /// * `datasource` - The data source string to parse
    ///
    /// # Returns
    /// A Result containing the parsed data source information
    ///
    /// # Example
    /// ```
    /// let mut context = ClientContext::new();
    /// let parsed = context.parse_datasource("tcp:myserver,1433")?;
    /// ```
    pub fn parse_datasource(&mut self, datasource: &str) -> TdsResult<ParsedDataSource> {
        let parsed = ParsedDataSource::parse(datasource, false)?;

        // Store the original data source string
        self.data_source = datasource.to_string();

        // Update transport context based on parsed data source
        self.transport_context = TransportContext::from_parsed_datasource(&parsed)?;

        // Store instance name for protocol resolution
        if !parsed.instance_name.is_empty() {
            self.database_instance = parsed.instance_name.clone();
        }

        Ok(parsed)
    }

    /// Parse a data source string with MultiSubnetFailover support
    ///
    /// Similar to parse_datasource but allows specifying MultiSubnetFailover option
    /// which restricts protocol selection to TCP only.
    ///
    /// # Arguments
    /// * `datasource` - The data source string to parse
    /// * `multi_subnet_failover` - Whether MultiSubnetFailover is enabled
    pub fn parse_datasource_with_options(
        &mut self,
        datasource: &str,
        multi_subnet_failover: bool,
    ) -> TdsResult<ParsedDataSource> {
        let parsed = ParsedDataSource::parse(datasource, multi_subnet_failover)?;

        // Store the original data source string
        self.data_source = datasource.to_string();

        // Update transport context based on parsed data source
        self.transport_context = TransportContext::from_parsed_datasource(&parsed)?;

        // Store instance name for protocol resolution
        if !parsed.instance_name.is_empty() {
            self.database_instance = parsed.instance_name.clone();
        }

        Ok(parsed)
    }
}

impl Clone for ClientContext {
    fn clone(&self) -> Self {
        ClientContext {
            application_intent: self.application_intent,
            application_name: self.application_name.clone(),
            attach_db_file: self.attach_db_file.clone(),
            change_password: self.change_password.clone(),
            connect_retry_count: self.connect_retry_count,
            connect_retry_interval: self.connect_retry_interval,
            connect_timeout: self.connect_timeout,
            database: self.database.clone(),
            data_source: self.data_source.clone(),
            keep_alive_in_ms: self.keep_alive_in_ms,
            keep_alive_interval_in_ms: self.keep_alive_interval_in_ms,
            database_instance: self.database_instance.clone(),
            enlist: self.enlist,
            encryption_options: self.encryption_options.clone(),
            failover_partner: self.failover_partner.clone(),
            ipaddress_preference: self.ipaddress_preference,
            language: self.language.clone(),
            library_name: self.library_name.clone(),
            driver_version: self.driver_version,
            auth_method_map: self.clone_auth_method_map(),
            mars_enabled: self.mars_enabled,
            multi_subnet_failover: self.multi_subnet_failover,
            ssrp_timeout_ms: self.ssrp_timeout_ms,
            new_password: self.new_password.clone(),
            packet_size: self.packet_size,
            password: self.password.clone(),
            pooling: self.pooling,
            replication: self.replication,
            tds_authentication_method: self.tds_authentication_method.clone(),
            user_instance: self.user_instance,
            user_name: self.user_name.clone(),
            workstation_id: self.workstation_id.clone(),
            server_spn: self.server_spn.clone(),
            access_token: self.access_token.clone(),
            transport_context: self.transport_context.clone(),
            vector_version: self.vector_version,
            column_encryption_setting: self.column_encryption_setting,
            column_encryption_key_store_providers: self
                .column_encryption_key_store_providers
                .clone(),
            cek_cache: self.cek_cache.clone(),
            user_agent: self.user_agent.clone(),
        }
    }
}

/// Protocol types for SQL Server connections
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Protocol {
    /// TCP/IP protocol (default)
    Tcp,
    /// Named Pipes protocol
    NamedPipe,
    /// Shared Memory protocol (local only)
    SharedMemory,
}

/// Transport protocol and endpoint for the connection.
#[derive(PartialEq, Clone, Debug)]
pub enum TransportContext {
    /// TCP/IP connection.
    Tcp {
        /// Network hostname.
        host: String,
        /// TCP port number.
        port: u16,
        /// Optional SQL Server instance name (e.g., "SQLEXPRESS").
        instance_name: Option<String>,
    },
    /// Named Pipe connection (`\\server\pipe\sql\query`).
    NamedPipe {
        /// Full UNC pipe path.
        pipe_name: String,
    },
    /// Shared Memory connection (local only).
    SharedMemory {
        /// SQL Server instance name.
        instance_name: String,
    },
    /// LocalDB connection (Windows only) with instance name
    /// Format: (localdb)\InstanceName
    #[cfg(windows)]
    LocalDB {
        /// LocalDB instance name.
        instance_name: String,
    },
}

impl TransportContext {
    /// Create a TCP TransportContext from a routing token.
    ///
    /// Routing tokens can legitimately contain both `host\instance` and a port,
    /// so this method splits on `\` directly instead of going through
    /// `ParsedDataSource::parse` (which drops the instance when a port is present).
    pub fn from_routing_token(host: String, port: u16) -> Self {
        let (network_host, instance_name) = match host.split_once('\\') {
            Some((host_part, instance)) => {
                let instance = if instance.is_empty() {
                    None
                } else {
                    Some(instance.to_string())
                };
                (host_part.to_string(), instance)
            }
            None => (host, None),
        };

        TransportContext::Tcp {
            host: network_host,
            port,
            instance_name,
        }
    }

    /// Get the server name from the transport context (hostname only, for internal use)
    pub fn get_server_name(&self) -> String {
        match self {
            TransportContext::Tcp { host, .. } => host.clone(),
            TransportContext::NamedPipe { pipe_name } => {
                // Extract server name from pipe path like \\.\pipe\sql\query or \\server\pipe\sql\query
                if pipe_name.starts_with("\\\\.\\") {
                    "localhost".to_string()
                } else if let Some(rest) = pipe_name.strip_prefix("\\\\") {
                    if let Some(idx) = rest.find('\\') {
                        rest[..idx].to_string()
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                }
            }
            TransportContext::SharedMemory { .. } => "localhost".to_string(),
            #[cfg(windows)]
            TransportContext::LocalDB { instance_name } => {
                format!("(localdb)\\{instance_name}")
            }
        }
    }

    /// Get the server name in DataSource format for Login7 packet.
    /// For TCP connections, this returns "host,port" or "host\instance,port" format.
    /// This matches SqlClient behavior where the client sends the full DataSource string
    /// back to the server, especially important for redirected connections.
    ///
    /// If `instance_name` is set (from routing token), formats as "host\instance,port".
    pub fn get_login_server_name(&self) -> String {
        match self {
            TransportContext::Tcp {
                host,
                port,
                instance_name,
            } => {
                // Derive full server name from host and optional instance_name
                if let Some(instance) = instance_name {
                    format!("{}\\{},{}", host, instance, port)
                } else {
                    format!("{},{}", host, port)
                }
            }
            // For non-TCP protocols, just return the server name
            _ => self.get_server_name(),
        }
    }

    /// Get the protocol type for this transport context
    pub fn get_protocol(&self) -> Protocol {
        match self {
            TransportContext::Tcp { .. } => Protocol::Tcp,
            TransportContext::NamedPipe { .. } => Protocol::NamedPipe,
            TransportContext::SharedMemory { .. } => Protocol::SharedMemory,
            #[cfg(windows)]
            TransportContext::LocalDB { .. } => Protocol::NamedPipe, // LocalDB uses named pipes internally
        }
    }

    /// Check if the connection is local
    pub fn is_local(&self) -> bool {
        match self {
            TransportContext::Tcp { host, .. } => {
                matches!(
                    host.to_lowercase().as_str(),
                    "." | "(local)" | "localhost" | "127.0.0.1" | "::1"
                )
            }
            TransportContext::NamedPipe { pipe_name } => pipe_name.starts_with("\\\\.\\"),
            TransportContext::SharedMemory { .. } => true,
            #[cfg(windows)]
            TransportContext::LocalDB { .. } => true, // LocalDB is always local
        }
    }

    /// Get the port for SPN construction (default 1433 for non-TCP)
    pub fn get_port(&self) -> u16 {
        match self {
            TransportContext::Tcp { port, .. } => *port,
            // For non-TCP protocols, use default SQL Server port for SPN
            _ => 1433,
        }
    }

    /// Create TransportContext from a parsed data source
    ///
    /// This method converts a ParsedDataSource into a TransportContext,
    /// determining the appropriate transport method based on the parsed data.
    pub fn from_parsed_datasource(parsed: &ParsedDataSource) -> TdsResult<Self> {
        use crate::error::Error;

        match parsed.get_protocol_type() {
            ProtocolType::Tcp => {
                let port = if !parsed.protocol_parameter.is_empty() {
                    parsed
                        .protocol_parameter
                        .parse::<u16>()
                        .map_err(|e| Error::ProtocolError(format!("Invalid port number: {}", e)))?
                } else {
                    1433 // Default SQL Server port
                };

                Ok(TransportContext::Tcp {
                    host: parsed.server_name.clone(),
                    port,
                    instance_name: None,
                })
            }
            ProtocolType::NamedPipe => {
                let pipe_name = if !parsed.protocol_parameter.is_empty() {
                    parsed.protocol_parameter.clone()
                } else if !parsed.instance_name.is_empty() {
                    // Build standard named pipe path
                    if parsed.instance_name.to_lowercase() == "default"
                        || parsed.instance_name.is_empty()
                    {
                        format!("\\\\{}\\pipe\\sql\\query", parsed.server_name)
                    } else {
                        format!(
                            "\\\\{}\\pipe\\MSSQL${}\\sql\\query",
                            parsed.server_name, parsed.instance_name
                        )
                    }
                } else {
                    // Default instance
                    format!("\\\\{}\\pipe\\sql\\query", parsed.server_name)
                };

                Ok(TransportContext::NamedPipe { pipe_name })
            }
            ProtocolType::SharedMemory => {
                let instance_name = if !parsed.instance_name.is_empty() {
                    parsed.instance_name.clone()
                } else {
                    "MSSQLSERVER".to_string()
                };

                Ok(TransportContext::SharedMemory { instance_name })
            }
            ProtocolType::Admin => {
                // DAC always uses TCP on port 1434 by default
                Ok(TransportContext::Tcp {
                    host: parsed.server_name.clone(),
                    port: 1434,
                    instance_name: None,
                })
            }
            ProtocolType::Auto => {
                // Auto-detect: prefer TCP with default port
                Ok(TransportContext::Tcp {
                    host: parsed.server_name.clone(),
                    port: 1433,
                    instance_name: None,
                })
            }
        }
    }

    /// Parse a server name string and return the appropriate TransportContext
    ///
    /// Supported formats:
    /// - `(localdb)\InstanceName` or `(localdb)/InstanceName` -> LocalDB (Windows only)
    /// - `\\server\pipe\path` -> NamedPipe
    /// - `lpc:InstanceName` -> SharedMemory
    /// - `hostname:port` -> Tcp
    /// - `hostname` -> Tcp with default_port
    pub fn parse_server_name(server_name: &str, default_port: u16) -> TransportContext {
        let server_lower = server_name.to_lowercase();

        // Check for LocalDB format: (localdb)\InstanceName or (localdb)/InstanceName
        #[cfg(windows)]
        if server_lower.starts_with("(localdb)\\") || server_lower.starts_with("(localdb)/") {
            let instance_name = server_name[10..].to_string(); // Skip "(localdb)\" or "(localdb)/"
            return TransportContext::LocalDB { instance_name };
        }

        // Check for Named Pipe format: \\server\pipe\...
        if server_name.starts_with("\\\\") {
            return TransportContext::NamedPipe {
                pipe_name: server_name.to_string(),
            };
        }

        // Check for Shared Memory format: lpc:InstanceName
        if server_lower.starts_with("lpc:") {
            let instance_name = server_name[4..].to_string(); // Skip "lpc:"
            return TransportContext::SharedMemory { instance_name };
        }

        // Parse TCP format: hostname or hostname,port
        // SQL Server connection strings use comma as the port separator (e.g., localhost,1433)
        if let Some(comma_idx) = server_name.rfind(',') {
            let host = server_name[..comma_idx].to_string();
            let port_str = &server_name[comma_idx + 1..];
            if let Ok(port) = port_str.parse::<u16>() {
                return TransportContext::Tcp {
                    host,
                    port,
                    instance_name: None,
                };
            }
        }

        // Default: treat as hostname without port
        TransportContext::Tcp {
            host: server_name.to_string(),
            port: default_port,
            instance_name: None,
        }
    }

    /// Check if this is a LocalDB connection (Windows only)
    #[cfg(windows)]
    pub fn is_localdb(&self) -> bool {
        matches!(self, TransportContext::LocalDB { .. })
    }

    /// Get the LocalDB instance name if this is a LocalDB connection (Windows only)
    #[cfg(windows)]
    pub fn get_localdb_instance(&self) -> Option<&str> {
        match self {
            TransportContext::LocalDB { instance_name } => Some(instance_name.as_str()),
            _ => None,
        }
    }
}

#[cfg(test)]
#[allow(deprecated)]
mod tests {
    use super::*;

    #[test]
    fn test_with_data_source_constructor() {
        let ctx = ClientContext::with_data_source("tcp:myserver,1433");
        assert_eq!(ctx.data_source, "tcp:myserver,1433");
        // Other defaults should still be set
        assert_eq!(ctx.connect_timeout, 15);
        assert_eq!(ctx.packet_size, 8000);
        assert_eq!(ctx.application_name, "TDSX Rust Client");
    }

    #[test]
    fn test_parse_datasource_sets_data_source() {
        let mut ctx = ClientContext::new();
        assert_eq!(ctx.data_source, ""); // Initially empty

        let _ = ctx.parse_datasource("tcp:myserver,1433");
        assert_eq!(ctx.data_source, "tcp:myserver,1433");
    }

    #[test]
    fn test_data_source_cloned() {
        let ctx = ClientContext::with_data_source("tcp:myserver,1433");
        let cloned = ctx.clone();
        assert_eq!(cloned.data_source, "tcp:myserver,1433");
    }

    #[test]
    fn test_default_workstation_id_truncation() {
        // Simulate a long hostname
        let long_hostname = "a".repeat(150);
        let truncated_hostname = long_hostname[..128].to_string();

        // Test the default_workstation_id function with a mock closure
        let result = ClientContext::default_workstation_id(|| {
            Ok(std::ffi::OsString::from(long_hostname.clone()))
        });
        assert_eq!(result, truncated_hostname);
    }

    #[test]
    fn test_tcp_transport_get_server_name() {
        let ctx = TransportContext::Tcp {
            host: "myserver.example.com".to_string(),
            port: 1433,
            instance_name: None,
        };
        assert_eq!(ctx.get_server_name(), "myserver.example.com");
        assert_eq!(ctx.get_protocol(), Protocol::Tcp);
        assert!(!ctx.is_local());
    }

    #[test]
    fn test_tcp_transport_localhost() {
        let ctx = TransportContext::Tcp {
            host: "localhost".to_string(),
            port: 1433,
            instance_name: None,
        };
        assert_eq!(ctx.get_server_name(), "localhost");
        assert!(ctx.is_local());
    }

    #[test]
    fn test_named_pipe_local() {
        let ctx = TransportContext::NamedPipe {
            pipe_name: "\\\\.\\pipe\\sql\\query".to_string(),
        };
        assert_eq!(ctx.get_server_name(), "localhost");
        assert_eq!(ctx.get_protocol(), Protocol::NamedPipe);
        assert!(ctx.is_local());
    }

    #[test]
    fn test_named_pipe_remote() {
        let ctx = TransportContext::NamedPipe {
            pipe_name: "\\\\myserver\\pipe\\sql\\query".to_string(),
        };
        assert_eq!(ctx.get_server_name(), "myserver");
        assert_eq!(ctx.get_protocol(), Protocol::NamedPipe);
        assert!(!ctx.is_local());
    }

    #[test]
    fn test_named_pipe_with_instance() {
        let ctx = TransportContext::NamedPipe {
            pipe_name: "\\\\myserver\\pipe\\MSSQL$SQLEXPRESS\\sql\\query".to_string(),
        };
        assert_eq!(ctx.get_server_name(), "myserver");
        assert_eq!(ctx.get_protocol(), Protocol::NamedPipe);
    }

    #[test]
    fn test_shared_memory() {
        let ctx = TransportContext::SharedMemory {
            instance_name: "MSSQLSERVER".to_string(),
        };
        assert_eq!(ctx.get_server_name(), "localhost");
        assert_eq!(ctx.get_protocol(), Protocol::SharedMemory);
        assert!(ctx.is_local());
    }

    #[test]
    fn test_shared_memory_with_instance() {
        let ctx = TransportContext::SharedMemory {
            instance_name: "SQLEXPRESS".to_string(),
        };
        assert_eq!(ctx.get_server_name(), "localhost");
        assert!(ctx.is_local());
    }

    #[test]
    fn test_transport_context_get_server_name() {
        // TCP
        let tcp_context = TransportContext::Tcp {
            host: "localhost".to_string(),
            port: 1433,
            instance_name: None,
        };
        assert_eq!(tcp_context.get_server_name(), "localhost");

        // Named Pipe
        let np_context = TransportContext::NamedPipe {
            pipe_name: r"\\server\pipe\sql\query".to_string(),
        };
        assert_eq!(np_context.get_server_name(), "server");

        let np_local_context = TransportContext::NamedPipe {
            pipe_name: r"\\.\pipe\sql\query".to_string(),
        };
        assert_eq!(np_local_context.get_server_name(), "localhost");

        // Shared Memory
        let sm_context = TransportContext::SharedMemory {
            instance_name: String::new(),
        };
        assert_eq!(sm_context.get_server_name(), "localhost");

        let sm_named_context = TransportContext::SharedMemory {
            instance_name: "SQLEXPRESS".to_string(),
        };
        assert_eq!(sm_named_context.get_server_name(), "localhost");
    }

    #[test]
    fn test_transport_context_is_local() {
        // TCP - not local
        let tcp_context = TransportContext::Tcp {
            host: "remote-server".to_string(),
            port: 1433,
            instance_name: None,
        };
        assert!(!tcp_context.is_local());

        // TCP - localhost
        let tcp_localhost = TransportContext::Tcp {
            host: "localhost".to_string(),
            port: 1433,
            instance_name: None,
        };
        assert!(tcp_localhost.is_local());

        // TCP - 127.0.0.1
        let tcp_loopback = TransportContext::Tcp {
            host: "127.0.0.1".to_string(),
            port: 1433,
            instance_name: None,
        };
        assert!(tcp_loopback.is_local());

        // Named Pipe with . (local)
        let np_local = TransportContext::NamedPipe {
            pipe_name: r"\\.\pipe\sql\query".to_string(),
        };
        assert!(np_local.is_local());

        // Named Pipe with remote server
        let np_remote = TransportContext::NamedPipe {
            pipe_name: r"\\remote-server\pipe\sql\query".to_string(),
        };
        assert!(!np_remote.is_local());

        // Shared Memory - always local
        let sm_context = TransportContext::SharedMemory {
            instance_name: String::new(),
        };
        assert!(sm_context.is_local());
    }

    #[test]
    fn test_transport_context_get_protocol() {
        // TCP
        let tcp_context = TransportContext::Tcp {
            host: "localhost".to_string(),
            port: 1433,
            instance_name: None,
        };
        assert!(matches!(tcp_context.get_protocol(), Protocol::Tcp));

        // Named Pipe
        let np_context = TransportContext::NamedPipe {
            pipe_name: r"\\.\pipe\sql\query".to_string(),
        };
        assert!(matches!(np_context.get_protocol(), Protocol::NamedPipe));

        // Shared Memory
        let sm_context = TransportContext::SharedMemory {
            instance_name: String::new(),
        };
        assert!(matches!(sm_context.get_protocol(), Protocol::SharedMemory));
    }

    // LocalDB parsing tests
    #[test]
    #[cfg(windows)]
    fn test_parse_server_name_localdb() {
        // Test basic LocalDB format with backslash
        let ctx = TransportContext::parse_server_name("(localdb)\\MSSQLLocalDB", 1433);
        assert!(ctx.is_localdb());
        assert_eq!(ctx.get_localdb_instance(), Some("MSSQLLocalDB"));
        assert_eq!(ctx.get_server_name(), "(localdb)\\MSSQLLocalDB");
        assert!(ctx.is_local());
        assert_eq!(ctx.get_protocol(), Protocol::NamedPipe);

        // Test LocalDB with forward slash
        let ctx2 = TransportContext::parse_server_name("(localdb)/v11.0", 1433);
        assert!(ctx2.is_localdb());
        assert_eq!(ctx2.get_localdb_instance(), Some("v11.0"));

        // Test case insensitivity
        let ctx3 = TransportContext::parse_server_name("(LocalDB)\\MyInstance", 1433);
        assert!(ctx3.is_localdb());
        assert_eq!(ctx3.get_localdb_instance(), Some("MyInstance"));

        // Test with uppercase
        let ctx4 = TransportContext::parse_server_name("(LOCALDB)\\TEST", 1433);
        assert!(ctx4.is_localdb());
        assert_eq!(ctx4.get_localdb_instance(), Some("TEST"));
    }

    #[test]
    fn test_parse_server_name_tcp() {
        // Simple hostname
        let ctx = TransportContext::parse_server_name("myserver", 1433);
        assert_eq!(ctx.get_server_name(), "myserver");
        assert_eq!(ctx.get_protocol(), Protocol::Tcp);
        if let TransportContext::Tcp { host, port, .. } = ctx {
            assert_eq!(host, "myserver");
            assert_eq!(port, 1433);
        } else {
            panic!("Expected Tcp variant");
        }

        // Hostname with port (SQL Server uses comma as separator)
        let ctx2 = TransportContext::parse_server_name("myserver,1434", 1433);
        if let TransportContext::Tcp { host, port, .. } = ctx2 {
            assert_eq!(host, "myserver");
            assert_eq!(port, 1434);
        } else {
            panic!("Expected Tcp variant");
        }

        // Hostname with domain
        let ctx3 = TransportContext::parse_server_name("sql.contoso.com", 1433);
        if let TransportContext::Tcp { host, port, .. } = ctx3 {
            assert_eq!(host, "sql.contoso.com");
            assert_eq!(port, 1433);
        } else {
            panic!("Expected Tcp variant");
        }

        // IP address with port
        let ctx4 = TransportContext::parse_server_name("192.168.1.100,5000", 1433);
        if let TransportContext::Tcp { host, port, .. } = ctx4 {
            assert_eq!(host, "192.168.1.100");
            assert_eq!(port, 5000);
        } else {
            panic!("Expected Tcp variant");
        }

        // localhost
        let ctx5 = TransportContext::parse_server_name("localhost", 1433);
        assert!(ctx5.is_local());
    }

    #[test]
    fn test_parse_server_name_named_pipe() {
        // Local named pipe
        let ctx = TransportContext::parse_server_name("\\\\.\\pipe\\sql\\query", 1433);
        assert_eq!(ctx.get_protocol(), Protocol::NamedPipe);
        assert_eq!(ctx.get_server_name(), "localhost");
        assert!(ctx.is_local());
        if let TransportContext::NamedPipe { pipe_name } = ctx {
            assert_eq!(pipe_name, "\\\\.\\pipe\\sql\\query");
        } else {
            panic!("Expected NamedPipe variant");
        }

        // Remote named pipe
        let ctx2 = TransportContext::parse_server_name("\\\\server\\pipe\\sql\\query", 1433);
        assert_eq!(ctx2.get_server_name(), "server");
        assert!(!ctx2.is_local());

        // Named pipe with instance
        let ctx3 = TransportContext::parse_server_name(
            "\\\\server\\pipe\\MSSQL$SQLEXPRESS\\sql\\query",
            1433,
        );
        assert_eq!(ctx3.get_server_name(), "server");
        if let TransportContext::NamedPipe { pipe_name } = ctx3 {
            assert_eq!(pipe_name, "\\\\server\\pipe\\MSSQL$SQLEXPRESS\\sql\\query");
        } else {
            panic!("Expected NamedPipe variant");
        }
    }

    #[test]
    fn test_parse_server_name_shared_memory() {
        // Shared memory default instance
        let ctx = TransportContext::parse_server_name("lpc:MSSQLSERVER", 1433);
        assert_eq!(ctx.get_protocol(), Protocol::SharedMemory);
        assert!(ctx.is_local());
        if let TransportContext::SharedMemory { instance_name } = ctx {
            assert_eq!(instance_name, "MSSQLSERVER");
        } else {
            panic!("Expected SharedMemory variant");
        }

        // Shared memory named instance
        let ctx2 = TransportContext::parse_server_name("lpc:SQLEXPRESS", 1433);
        if let TransportContext::SharedMemory { instance_name } = ctx2 {
            assert_eq!(instance_name, "SQLEXPRESS");
        } else {
            panic!("Expected SharedMemory variant");
        }

        // Case insensitive
        let ctx3 = TransportContext::parse_server_name("LPC:MyInstance", 1433);
        assert_eq!(ctx3.get_protocol(), Protocol::SharedMemory);
    }

    #[test]
    #[cfg(windows)]
    fn test_localdb_helper_methods() {
        let localdb_ctx = TransportContext::LocalDB {
            instance_name: "TestInstance".to_string(),
        };
        assert!(localdb_ctx.is_localdb());
        assert_eq!(localdb_ctx.get_localdb_instance(), Some("TestInstance"));
        assert!(localdb_ctx.is_local());
        assert_eq!(localdb_ctx.get_protocol(), Protocol::NamedPipe);

        let tcp_ctx = TransportContext::Tcp {
            host: "localhost".to_string(),
            port: 1433,
            instance_name: None,
        };
        assert!(!tcp_ctx.is_localdb());
        assert_eq!(tcp_ctx.get_localdb_instance(), None);
    }

    #[test]
    fn test_parse_special_cases() {
        // Dot notation (local)
        let ctx = TransportContext::parse_server_name(".", 1433);
        if let TransportContext::Tcp { host, .. } = &ctx {
            assert_eq!(host, ".");
            assert!(ctx.is_local());
        } else {
            panic!("Expected Tcp variant");
        }

        // (local) notation
        let ctx2 = TransportContext::parse_server_name("(local)", 1433);
        assert!(ctx2.is_local());

        // IPv6 loopback
        let ctx3 = TransportContext::parse_server_name("::1", 1433);
        assert!(ctx3.is_local());
    }

    #[test]
    fn test_client_context_keep_alive_defaults() {
        let ctx = ClientContext::new();
        // Default keep_alive_in_ms should be 30 seconds (30000 ms) per SQL Server defaults
        assert_eq!(ctx.keep_alive_in_ms, 30_000);
        // Default keep_alive_interval_in_ms should be 1 second (1000 ms) per SQL Server defaults
        assert_eq!(ctx.keep_alive_interval_in_ms, 1_000);
    }

    #[test]
    fn test_client_context_keep_alive_custom_values() {
        let mut ctx = ClientContext::new();
        ctx.keep_alive_in_ms = 60_000; // 60 seconds
        ctx.keep_alive_interval_in_ms = 5_000; // 5 seconds

        assert_eq!(ctx.keep_alive_in_ms, 60_000);
        assert_eq!(ctx.keep_alive_interval_in_ms, 5_000);
    }

    #[test]
    fn test_client_context_keep_alive_clone() {
        let mut ctx = ClientContext::new();
        ctx.keep_alive_in_ms = 45_000;
        ctx.keep_alive_interval_in_ms = 2_000;

        let cloned = ctx.clone();
        assert_eq!(cloned.keep_alive_in_ms, 45_000);
        assert_eq!(cloned.keep_alive_interval_in_ms, 2_000);
    }

    #[test]
    fn test_client_context_keep_alive_zero_values() {
        // Test that zero values are allowed (disables keep-alive on some systems)
        let mut ctx = ClientContext::new();
        ctx.keep_alive_in_ms = 0;
        ctx.keep_alive_interval_in_ms = 0;

        assert_eq!(ctx.keep_alive_in_ms, 0);
        assert_eq!(ctx.keep_alive_interval_in_ms, 0);
    }

    #[test]
    fn test_client_context_keep_alive_max_values() {
        // Test maximum u32 values
        let mut ctx = ClientContext::new();
        ctx.keep_alive_in_ms = u32::MAX;
        ctx.keep_alive_interval_in_ms = u32::MAX;

        assert_eq!(ctx.keep_alive_in_ms, u32::MAX);
        assert_eq!(ctx.keep_alive_interval_in_ms, u32::MAX);
    }

    #[test]
    fn test_packet_size_validation_valid_min() {
        let mut ctx = ClientContext::new();
        ctx.packet_size = 512; // Minimum valid packet size
        assert!(ctx.validate().is_ok());
    }

    #[test]
    fn test_packet_size_validation_valid_max() {
        let mut ctx = ClientContext::new();
        ctx.packet_size = 32768; // Maximum valid packet size
        assert!(ctx.validate().is_ok());
    }

    #[test]
    fn test_packet_size_validation_valid_default() {
        let ctx = ClientContext::new();
        // Default packet_size is 8000, which should be valid
        assert!(ctx.validate().is_ok());
    }

    #[test]
    fn test_packet_size_validation_invalid_too_small() {
        let mut ctx = ClientContext::new();
        ctx.packet_size = 511; // Below minimum
        let result = ctx.validate();
        assert!(result.is_err());
        if let Err(Error::UsageError(msg)) = result {
            assert!(msg.contains("Invalid packet size"));
            assert!(msg.contains("511"));
        } else {
            panic!("Expected UsageError");
        }
    }

    #[test]
    fn test_packet_size_validation_invalid_too_large() {
        let mut ctx = ClientContext::new();
        ctx.packet_size = 32769; // Above maximum
        let result = ctx.validate();
        assert!(result.is_err());
        if let Err(Error::UsageError(msg)) = result {
            assert!(msg.contains("Invalid packet size"));
            assert!(msg.contains("32769"));
        } else {
            panic!("Expected UsageError");
        }
    }

    #[test]
    fn test_custom_validator() {
        struct CustomValidator;
        impl ClientContextValidator for CustomValidator {
            fn validate(&self, _context: &ClientContext) -> TdsResult<()> {
                Err(Error::UsageError("Custom validation failed".to_string()))
            }
        }

        let ctx = ClientContext::new();
        let result = ctx.validate_with(&CustomValidator);
        assert!(result.is_err());
        if let Err(Error::UsageError(msg)) = result {
            assert_eq!(msg, "Custom validation failed");
        } else {
            panic!("Expected UsageError");
        }
    }

    #[test]
    fn test_driver_version_encode() {
        let v = DriverVersion::new(1, 2, 3);
        // [major(1)][minor(2)][build(3)] = 0x01020003
        assert_eq!(v.encode(), 0x01020003);
    }

    #[test]
    fn test_driver_version_encode_max_values() {
        let v = DriverVersion::new(255, 255, 65535);
        // [255][255][65535] = 0xFFFFFFFF
        assert_eq!(v.encode(), 0xFFFFFFFF_u32 as i32);
    }

    #[test]
    fn test_driver_version_encode_zero() {
        let v = DriverVersion::new(0, 0, 0);
        assert_eq!(v.encode(), 0);
    }

    #[test]
    fn test_driver_version_from_cargo() {
        let v = DriverVersion::from_cargo_version();
        // Should parse the crate version "0.1.0"
        assert_eq!(v, DriverVersion::new(0, 1, 0));
        assert_eq!(v.to_string(), env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn test_driver_version_default_in_context() {
        let ctx = ClientContext::new();
        assert_eq!(ctx.driver_version, DriverVersion::from_cargo_version());
        assert_ne!(ctx.encode_driver_version(), 0);
    }

    #[test]
    fn test_driver_version_custom_override() {
        let mut ctx = ClientContext::new();
        ctx.driver_version = DriverVersion::new(2, 5, 1234);
        // [major(2)][minor(5)][build(1234)] = 0x020504D2
        assert_eq!(ctx.encode_driver_version(), 0x020504D2);
    }

    #[test]
    fn test_driver_version_display() {
        assert_eq!(DriverVersion::new(1, 2, 3).to_string(), "1.2.3");
        assert_eq!(DriverVersion::new(0, 0, 0).to_string(), "0.0.0");
        assert_eq!(
            DriverVersion::new(255, 255, 65535).to_string(),
            "255.255.65535"
        );
    }

    #[test]
    fn test_default_library_name() {
        let ctx = ClientContext::new();
        assert_eq!(ctx.library_name, "MS-TDS");
    }

    #[test]
    fn entra_id_token_factory_missing_returns_error() {
        let mut ctx = ClientContext::with_data_source("localhost");
        ctx.tds_authentication_method = TdsAuthenticationMethod::ActiveDirectoryIntegrated;
        let result = ctx.entra_id_token_factory();
        assert!(result.is_err());
        let err = result.err().unwrap().to_string();
        assert!(err.contains("ActiveDirectoryIntegrated"));
        assert!(err.contains("not supported"));
    }

    #[test]
    fn test_user_agent_default() {
        let ua = UserAgent::default();
        assert_eq!(ua.driver_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(ua.library_name, "MS-TDS");
    }

    #[test]
    fn test_client_context_user_agent_setters() {
        let mut ctx = ClientContext::new();

        ctx.user_agent.set_library_name("AnotherLib".to_string());
        assert_eq!(ctx.user_agent.library_name, "AnotherLib");

        ctx.user_agent.set_driver_version("7.8.9".to_string());
        assert_eq!(ctx.user_agent.driver_version, "7.8.9");

        ctx.user_agent.set_runtime("1.80.0".to_string());
        assert_eq!(ctx.user_agent.runtime, "1.80.0");
    }

    #[test]
    fn from_routing_token_host_only() {
        let ctx =
            TransportContext::from_routing_token("myhost.database.windows.net".to_string(), 1433);
        match &ctx {
            TransportContext::Tcp {
                host,
                port,
                instance_name,
            } => {
                assert_eq!(host, "myhost.database.windows.net");
                assert_eq!(*port, 1433);
                assert!(instance_name.is_none());
            }
            _ => panic!("Expected Tcp variant"),
        }
        assert_eq!(
            ctx.get_login_server_name(),
            "myhost.database.windows.net,1433"
        );
    }

    #[test]
    fn from_routing_token_with_instance() {
        let ctx = TransportContext::from_routing_token(
            "myhost.pbidedicated.windows.net\\INSTANCE-dw".to_string(),
            1433,
        );
        match &ctx {
            TransportContext::Tcp {
                host,
                port,
                instance_name,
            } => {
                assert_eq!(host, "myhost.pbidedicated.windows.net");
                assert_eq!(*port, 1433);
                assert_eq!(instance_name.as_deref(), Some("INSTANCE-dw"));
            }
            _ => panic!("Expected Tcp variant"),
        }
        assert_eq!(
            ctx.get_login_server_name(),
            "myhost.pbidedicated.windows.net\\INSTANCE-dw,1433"
        );
    }
}
