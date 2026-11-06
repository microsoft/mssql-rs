// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.
// Portions derived from saurabh500/mssql-tiberius-bridge (MIT).

//! Connection configuration builder mirroring tiberius' `Config` API.
//!
//! Use [`Config`] to build connection settings with a fluent API, then pass
//! to [`Client::connect()`](crate::Client::connect) or
//! [`TdsManager::new()`](crate::TdsManager::new) for pooling.
//!
//! # Example
//!
//! ```rust,no_run
//! use mssql_rs::{Config, AuthMethod, EncryptionLevel};
//!
//! let mut cfg = Config::new();
//! cfg.host("db.example.com")
//!    .port(1433)
//!    .database("mydb")
//!    .authentication(AuthMethod::sql_server("sa", std::env::var("SQL_PASSWORD").expect("set SQL_PASSWORD")))
//!    .encryption(EncryptionLevel::Required)
//!    .trust_cert();
//! ```

use mssql_tds::connection::client_context::{
    ClientContext, DriverVersion, TdsAuthenticationMethod,
};
use mssql_tds::core::EncryptionSetting;
use std::fmt;
use std::path::PathBuf;

/// The driver name sent in the TDS Login7 packet and UserAgent feature extension.
/// Follows the MS driver naming convention (e.g., `MS-TDS`, `MS-PYTHON`).
const DRIVER_NAME: &str = "MS-MSSQL-RS";

fn driver_version() -> DriverVersion {
    DriverVersion::new(
        env!("CARGO_PKG_VERSION_MAJOR")
            .parse()
            .expect("driver major version exceeds the TDS limit"),
        env!("CARGO_PKG_VERSION_MINOR")
            .parse()
            .expect("driver minor version exceeds the TDS limit"),
        env!("CARGO_PKG_VERSION_PATCH")
            .parse()
            .expect("driver patch version exceeds the TDS limit"),
    )
}

/// TLS encryption level for the connection.
///
/// Controls whether and how TLS is negotiated with SQL Server.
/// Mirrors tiberius' `EncryptionLevel`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncryptionLevel {
    /// Don't request encryption from the client side. The server may still
    /// require it (TDS pre-login negotiation), in which case the connection
    /// upgrades to TLS regardless.
    ///
    /// **Alias of [`NotSupported`](Self::NotSupported)**: both variants map to
    /// the same underlying `EncryptionSetting::PreferOff` in `mssql-tds`. The
    /// pair exists for parity with tiberius' `EncryptionLevel`; pick whichever
    /// reads better at the call site.
    Off,
    /// Encrypt the entire connection (default).
    On,
    /// Advertise that the client does not support encryption. The server may
    /// still force TLS on the connection.
    ///
    /// **Alias of [`Off`](Self::Off)**: both variants map to
    /// `EncryptionSetting::PreferOff` in `mssql-tds`. Kept for tiberius API
    /// parity.
    NotSupported,
    /// Require encryption; fail if the server doesn't support it.
    Required,
    /// TDS 8.0 strict mode — performs the TLS handshake **before** the TDS
    /// pre-login packet, so the entire wire conversation (including pre-login)
    /// runs inside TLS. This eliminates the pre-login downgrade window and is
    /// required by some Azure SQL configurations (e.g. `Encrypt=Strict` in the
    /// .NET driver).
    ///
    /// Notes specific to `Strict`:
    /// - The TLS handshake uses ALPN with the `tds/8.0` protocol id, as
    ///   required by the TDS 8.0 specification.
    /// - Server certificate validation is **always enforced**; calling
    ///   [`Config::trust_cert`] is ignored under `Strict` mode.
    /// - [`Config::trust_cert_ca`] (custom CA bundle) and
    ///   [`Config::host_name_in_certificate`] (SNI override) still apply.
    Strict,
}

/// Network transport used to reach SQL Server.
///
/// The bridge defaults to [`Transport::Tcp`]. Named Pipes and Shared Memory
/// are exposed for parity with ODBC / .NET / tiberius and are only usable
/// against a SQL Server that has the corresponding SNI provider enabled.
///
/// **Platform note:** Named Pipes and Shared Memory are Windows-only at the
/// `mssql-tds` layer. The setters are available on all platforms so the same
/// code compiles cross-platform, but `Client::connect()` will fail at runtime
/// on Linux / macOS with a `ProtocolError` from `mssql-tds` when these
/// transports are selected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// TCP/IP — the default. Endpoint is built from
    /// [`Config::host`] / [`Config::port`] / [`Config::instance_name`].
    Tcp,
    /// Named Pipes (Windows). The optional `pipe` field overrides the pipe
    /// path; when `None`, the bridge synthesizes the standard path
    /// `\\<host>\pipe\sql\query` for the default instance, or
    /// `\\<host>\pipe\MSSQL$<instance>\sql\query` when
    /// [`Config::instance_name`] is set. A leading `.` for `host` resolves
    /// to the local machine, matching SQL Server convention.
    NamedPipe {
        /// Explicit pipe path (e.g. `\\\\.\\pipe\\sql\\query`). When `None`,
        /// the bridge derives one from `host` and `instance_name`.
        pipe: Option<String>,
    },
    /// Shared Memory / Local Procedure Call (Windows-only, local only).
    /// Endpoint is built from [`Config::host`] (typically `.` or `localhost`)
    /// plus optional [`Config::instance_name`].
    SharedMemory,
}

/// Authentication method for SQL Server connections.
///
/// # Variants
///
/// - [`SqlServer`](Self::SqlServer) — username/password authentication (most common)
/// - [`Integrated`](Self::Integrated) — Windows SSPI or Kerberos/GSSAPI
#[derive(Clone)]
pub enum AuthMethod {
    /// SQL Server authentication with username and password.
    SqlServer { user: String, password: String },
    /// Windows Integrated authentication (Kerberos/NTLM).
    Integrated,
    /// Microsoft Entra ID (Azure AD) federated authentication using a
    /// pre-acquired access token (JWT). Acquire the token from MSAL /
    /// `azure_identity` / `oauth2`, scoped for `https://database.windows.net/.default`.
    AadToken { token: String },
}

impl fmt::Debug for AuthMethod {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SqlServer { user, .. } => formatter
                .debug_struct("SqlServer")
                .field("user", user)
                .field("password", &"<REDACTED>")
                .finish(),
            Self::Integrated => formatter.write_str("Integrated"),
            Self::AadToken { .. } => formatter
                .debug_struct("AadToken")
                .field("token", &"<REDACTED>")
                .finish(),
        }
    }
}

impl AuthMethod {
    /// Create SQL Server authentication credentials.
    pub fn sql_server(user: impl Into<String>, password: impl Into<String>) -> Self {
        AuthMethod::SqlServer {
            user: user.into(),
            password: password.into(),
        }
    }

    /// Create Windows Integrated authentication.
    pub fn integrated() -> Self {
        AuthMethod::Integrated
    }

    /// Create AAD/Entra ID federated authentication from a pre-acquired
    /// access token (JWT). Mirrors tiberius' `AuthMethod::aad_token`.
    pub fn aad_token(token: impl Into<String>) -> Self {
        AuthMethod::AadToken {
            token: token.into(),
        }
    }
}

/// Fluent connection configuration builder, mirroring tiberius' `Config`.
///
/// All settings have sensible defaults — at minimum you need to set
/// credentials via [`authentication()`](Self::authentication).
///
/// # Defaults
///
/// | Setting | Default |
/// |---------|----------|
/// | Host | `localhost` |
/// | Port | `1433` |
/// | Database | `master` |
/// | Encryption | [`On`](EncryptionLevel::On) |
/// | Trust cert | `false` |
#[derive(Debug, Clone)]
pub struct Config {
    host: String,
    port: u16,
    database: String,
    auth: AuthMethod,
    trust_cert: bool,
    server_certificate: Option<PathBuf>,
    host_name_in_certificate: Option<String>,
    readonly: bool,
    encryption: EncryptionLevel,
    application_name: Option<String>,
    instance_name: Option<String>,
    client_name: Option<String>,
    send_string_parameters_as_unicode: bool,
    multi_subnet_failover: bool,
    transport: Transport,
}

impl Config {
    /// Create a new `Config` with defaults (`localhost:1433`, `master` database, empty SQL auth).
    pub fn new() -> Self {
        Config {
            host: "localhost".to_string(),
            port: 1433,
            database: "master".to_string(),
            auth: AuthMethod::SqlServer {
                user: String::new(),
                password: String::new(),
            },
            trust_cert: false,
            server_certificate: None,
            host_name_in_certificate: None,
            readonly: false,
            encryption: EncryptionLevel::On,
            application_name: None,
            instance_name: None,
            client_name: None,
            send_string_parameters_as_unicode: true,
            multi_subnet_failover: false,
            transport: Transport::Tcp,
        }
    }

    /// Set the server hostname or IP address.
    pub fn host(&mut self, host: impl Into<String>) -> &mut Self {
        self.host = host.into();
        self
    }

    /// Set the server port (default: 1433).
    pub fn port(&mut self, port: u16) -> &mut Self {
        self.port = port;
        self
    }

    /// Set the default database to connect to.
    pub fn database(&mut self, database: impl Into<String>) -> &mut Self {
        self.database = database.into();
        self
    }

    /// Set the authentication method (see [`AuthMethod`]).
    pub fn authentication(&mut self, auth: AuthMethod) -> &mut Self {
        self.auth = auth;
        self
    }

    /// Trust the server's TLS certificate without validation.
    ///
    /// **Use only for development** — in production, configure proper
    /// certificate validation.
    pub fn trust_cert(&mut self) -> &mut Self {
        self.trust_cert = true;
        self
    }

    /// Pin the server's TLS certificate to the DER- or PEM-encoded X.509
    /// file at `path`.
    ///
    /// When set, the driver bypasses standard CA chain validation and
    /// performs an exact binary match between the file and the certificate
    /// presented by the server. Mirrors tiberius' `Config::trust_cert_ca`.
    ///
    /// # Interaction with [`trust_cert`](Self::trust_cert)
    ///
    /// `trust_cert_ca` **supersedes** `trust_cert`. The bridge forwards both
    /// fields to `mssql-tds`, which logs a warning and uses the pinned
    /// certificate (`ServerCertificate takes precedence`). Net effect:
    /// validation is performed against the pinned cert, and `trust_cert` has
    /// no observable effect when both are set.
    ///
    /// # Mutual exclusion with [`host_name_in_certificate`](Self::host_name_in_certificate)
    ///
    /// `trust_cert_ca` and `host_name_in_certificate` are **mutually
    /// exclusive**: setting both causes `mssql-tds` to fail the TLS handshake
    /// with a usage error. Pin a cert *or* override the SNI hostname, not both.
    ///
    /// # Example
    ///
    /// ```rust,no_run
    /// use mssql_rs::Config;
    ///
    /// let mut cfg = Config::new();
    /// cfg.host("db.internal").trust_cert_ca("/etc/ssl/private-ca.pem");
    /// ```
    pub fn trust_cert_ca(&mut self, path: impl Into<PathBuf>) -> &mut Self {
        self.server_certificate = Some(path.into());
        self
    }

    /// Override the hostname used for TLS certificate name validation.
    pub fn host_name_in_certificate(&mut self, name: impl Into<String>) -> &mut Self {
        self.host_name_in_certificate = Some(name.into());
        self
    }

    /// Set the client workstation name sent in the Login7 packet.
    pub fn client_name(&mut self, name: impl Into<String>) -> &mut Self {
        self.client_name = Some(name.into());
        self
    }

    /// Controls whether `&str` and `String` parameters are sent as NVARCHAR.
    ///
    /// Defaults to `true`. Set to `false` to send string parameters as VARCHAR.
    pub fn send_string_parameters_as_unicode(&mut self, enabled: bool) -> &mut Self {
        self.send_string_parameters_as_unicode = enabled;
        self
    }

    pub(crate) fn string_parameters_as_unicode(&self) -> bool {
        self.send_string_parameters_as_unicode
    }

    /// Send `ApplicationIntent=ReadOnly` in the login (mirrors tiberius'
    /// `Config::readonly`). Required for routing to a readable secondary in
    /// an Always On availability group / Azure SQL geo-replica.
    ///
    /// Defaults to `ReadWrite`.
    pub fn readonly(&mut self, readonly: bool) -> &mut Self {
        self.readonly = readonly;
        self
    }

    /// Set the TLS encryption level.
    pub fn encryption(&mut self, level: EncryptionLevel) -> &mut Self {
        self.encryption = level;
        self
    }

    /// Enable `MultiSubnetFailover` (MSF) connection mode.
    ///
    /// With SQL Server Always On Availability Groups whose listener spans
    /// multiple subnets, the listener DNS record contains every replica IP.
    /// With MSF enabled, the client resolves all A/AAAA records and races
    /// `TcpStream::connect` against them in parallel, taking the first that
    /// completes the TCP handshake. This drives sub-second failover when a
    /// replica becomes unreachable.
    ///
    /// Notes:
    /// - MSF is only meaningful for **TCP** connections. mssql-tds rejects
    ///   the combination with Named Pipes / Shared Memory / LocalDB.
    /// - Off by default (single-target connect, the typical case).
    /// - Mirrors Microsoft .NET's `MultiSubnetFailover=true` connection
    ///   string keyword.
    pub fn multi_subnet_failover(&mut self, enabled: bool) -> &mut Self {
        self.multi_subnet_failover = enabled;
        self
    }

    /// Returns whether `MultiSubnetFailover` is enabled on this config.
    pub fn is_multi_subnet_failover(&self) -> bool {
        self.multi_subnet_failover
    }

    /// Use the Named Pipes transport instead of TCP.
    ///
    /// Pass `Some("\\\\.\\pipe\\sql\\query")` to force a specific pipe path,
    /// or `None` to let the bridge synthesize the standard pipe path from
    /// [`host`](Self::host) and [`instance_name`](Self::instance_name):
    ///
    /// - default instance: `\\<host>\pipe\sql\query`
    /// - named instance:   `\\<host>\pipe\MSSQL$<instance>\sql\query`
    ///
    /// When `host` is `.` it is mapped to the local machine, matching SQL
    /// Server / SNI convention.
    ///
    /// **Windows-only** at the `mssql-tds` layer; calling
    /// [`Client::connect`](crate::Client::connect) on Linux / macOS with
    /// Named Pipes selected will fail at runtime. The setter compiles
    /// everywhere for cross-platform code.
    ///
    /// Mirrors prisma/tiberius#131 / .NET's `np:` connection-string prefix.
    pub fn named_pipe(&mut self, pipe: Option<impl Into<String>>) -> &mut Self {
        self.transport = Transport::NamedPipe {
            pipe: pipe.map(Into::into),
        };
        self
    }

    /// Use the Shared Memory (LPC) transport instead of TCP.
    ///
    /// Endpoint is built from [`host`](Self::host) (typically `.` or
    /// `localhost`) plus optional [`instance_name`](Self::instance_name).
    ///
    /// **Windows-only and local-only** at the `mssql-tds` layer. Calling
    /// [`Client::connect`](crate::Client::connect) on Linux / macOS, or with
    /// a non-local `host`, will fail at runtime.
    ///
    /// Mirrors .NET's `lpc:` connection-string prefix.
    pub fn shared_memory(&mut self) -> &mut Self {
        self.transport = Transport::SharedMemory;
        self
    }

    /// Set the transport explicitly. Most callers should use the
    /// [`named_pipe`](Self::named_pipe) / [`shared_memory`](Self::shared_memory)
    /// helpers, or leave it at the default [`Transport::Tcp`].
    pub fn transport(&mut self, transport: Transport) -> &mut Self {
        self.transport = transport;
        self
    }

    /// Returns the configured transport (default [`Transport::Tcp`]).
    pub fn get_transport(&self) -> &Transport {
        &self.transport
    }

    /// Set the application name reported to the server in `sys.dm_exec_sessions`.
    pub fn application_name(&mut self, name: impl Into<String>) -> &mut Self {
        self.application_name = Some(name.into());
        self
    }

    /// Set the named instance (e.g., `SQLEXPRESS`).
    ///
    /// When set, the client uses SQL Browser to resolve the port.
    pub fn instance_name(&mut self, name: impl Into<String>) -> &mut Self {
        self.instance_name = Some(name.into());
        self
    }

    /// Get the address as `host:port` (useful for logging).
    pub fn get_addr(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Build the TDS datasource string used by `TdsConnectionProvider`.
    ///
    /// Format depends on [`get_transport`](Self::get_transport):
    ///
    /// - **TCP** (default):
    ///   - `tcp:host,port` — explicit port
    ///   - `tcp:host\instance` — when [`instance_name`](Self::instance_name)
    ///     is set the port is omitted so `mssql-tds` runs SSRP / SQL Browser
    ///   - Per MDAC convention, including both an explicit port and an
    ///     instance name causes the instance to be silently ignored, so the
    ///     bridge drops the port whenever an instance is specified.
    /// - **Named Pipes**:
    ///   - `np:<pipe-path>` — when an explicit pipe path was supplied
    ///   - `np:\\host\pipe\sql\query` — derived (default instance)
    ///   - `np:\\host\pipe\MSSQL$<instance>\sql\query` — derived (named instance)
    ///   - `host` of `.` is rewritten to the local machine name token
    ///     `\\.\pipe\…` per SQL Server convention.
    /// - **Shared Memory**:
    ///   - `lpc:host` (default instance) or `lpc:host\instance`
    pub fn datasource_string(&self) -> String {
        match &self.transport {
            Transport::Tcp => {
                if let Some(ref inst) = self.instance_name {
                    format!("tcp:{}\\{}", self.host, inst)
                } else {
                    format!("tcp:{},{}", self.host, self.port)
                }
            }
            Transport::NamedPipe { pipe: Some(p) } => format!("np:{p}"),
            Transport::NamedPipe { pipe: None } => {
                let pipe_host = if self.host == "." || self.host == "localhost" {
                    "."
                } else {
                    self.host.as_str()
                };
                match self.instance_name {
                    Some(ref inst) => format!("np:\\\\{pipe_host}\\pipe\\MSSQL${inst}\\sql\\query"),
                    None => format!("np:\\\\{pipe_host}\\pipe\\sql\\query"),
                }
            }
            Transport::SharedMemory => match self.instance_name {
                Some(ref inst) => format!("lpc:{}\\{}", self.host, inst),
                None => format!("lpc:{}", self.host),
            },
        }
    }

    /// Convert to an mssql-tds `ClientContext`.
    ///
    /// Sets the driver identity so SQL Server can distinguish connections
    /// from `mssql-rs` in `sys.dm_exec_sessions` and telemetry:
    ///
    /// - **Library name** (Login7 `ibCltIntName`): `MS-MSSQL-RS`
    /// - **UserAgent feature extension** (TDS 0x10):
    ///   `1|MS-MSSQL-RS|{version}|{arch}|{os}|{os_details}|...`
    /// - **Driver version** (`client_prog_ver`): crate major.minor.build
    pub fn to_client_context(&self) -> ClientContext {
        let mut ctx = ClientContext::default();
        ctx.database = self.database.clone();

        match &self.auth {
            AuthMethod::SqlServer { user, password } => {
                ctx.user_name = user.clone();
                ctx.password = password.clone();
            }
            AuthMethod::Integrated => {
                ctx.tds_authentication_method = TdsAuthenticationMethod::SSPI;
            }
            AuthMethod::AadToken { token } => {
                ctx.tds_authentication_method = TdsAuthenticationMethod::AccessToken;
                ctx.access_token = Some(token.clone());
            }
        }

        ctx.encryption_options.trust_server_certificate = self.trust_cert;
        ctx.encryption_options.server_certificate = self.server_certificate.clone();
        ctx.encryption_options.host_name_in_cert = self.host_name_in_certificate.clone();

        match self.encryption {
            EncryptionLevel::Off => ctx.encryption_options.mode = EncryptionSetting::PreferOff,
            EncryptionLevel::On => ctx.encryption_options.mode = EncryptionSetting::On,
            EncryptionLevel::NotSupported => {
                ctx.encryption_options.mode = EncryptionSetting::PreferOff
            }
            EncryptionLevel::Required => ctx.encryption_options.mode = EncryptionSetting::Required,
            EncryptionLevel::Strict => ctx.encryption_options.mode = EncryptionSetting::Strict,
        }

        if let Some(ref app) = self.application_name {
            ctx.application_name = app.clone();
        }

        if let Some(ref client_name) = self.client_name {
            ctx.workstation_id = client_name.clone();
        }

        // ── Driver identity ──
        // Login7 client interface name
        ctx.library_name = DRIVER_NAME.to_string();
        // Login7 client_prog_ver from this crate's version
        ctx.driver_version = driver_version();

        ctx.application_intent = if self.readonly {
            mssql_tds::message::login_options::ApplicationIntent::ReadOnly
        } else {
            mssql_tds::message::login_options::ApplicationIntent::ReadWrite
        };

        ctx.multi_subnet_failover = Some(self.multi_subnet_failover);

        // UserAgent feature extension (TDS 0x10)
        ctx.user_agent.set_library_name(DRIVER_NAME.to_string());
        ctx.user_agent
            .set_driver_version(env!("CARGO_PKG_VERSION").to_string());
        ctx
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use crate::config::*;
    use mssql_tds::connection::client_context::TdsAuthenticationMethod;

    #[test]
    fn default_config() {
        let cfg = Config::new();
        assert_eq!(cfg.get_addr(), "localhost:1433");
        assert_eq!(cfg.datasource_string(), "tcp:localhost,1433");
    }

    #[test]
    fn fluent_builder() {
        let password = uuid::Uuid::new_v4().to_string();
        let mut cfg = Config::new();
        cfg.host("db.example.com")
            .port(1445)
            .database("mydb")
            .authentication(AuthMethod::sql_server("sa", &password))
            .trust_cert()
            .encryption(EncryptionLevel::Required);
        assert_eq!(cfg.get_addr(), "db.example.com:1445");
        assert_eq!(cfg.datasource_string(), "tcp:db.example.com,1445");

        let ctx = cfg.to_client_context();
        assert_eq!(ctx.user_name, "sa");
        assert_eq!(ctx.password, password);
        assert_eq!(ctx.database, "mydb");
        assert!(ctx.encryption_options.trust_server_certificate);
    }

    #[test]
    fn trust_cert_ca_sets_server_certificate() {
        use std::path::Path;

        let mut cfg = Config::new();
        cfg.trust_cert_ca("/etc/ssl/ca.pem");
        let ctx = cfg.to_client_context();
        assert_eq!(
            ctx.encryption_options.server_certificate.as_deref(),
            Some(Path::new("/etc/ssl/ca.pem"))
        );
        assert!(!ctx.encryption_options.trust_server_certificate);
    }

    #[test]
    fn trust_cert_ca_independent_of_trust_cert() {
        use std::path::Path;

        let mut cfg = Config::new();
        cfg.trust_cert().trust_cert_ca("/tmp/ca.pem");
        let ctx = cfg.to_client_context();
        assert!(ctx.encryption_options.trust_server_certificate);
        assert_eq!(
            ctx.encryption_options.server_certificate.as_deref(),
            Some(Path::new("/tmp/ca.pem"))
        );
    }

    #[test]
    fn readonly_sets_application_intent() {
        use mssql_tds::message::login_options::ApplicationIntent;
        let mut cfg = Config::new();
        assert_eq!(
            cfg.to_client_context().application_intent,
            ApplicationIntent::ReadWrite
        );
        cfg.readonly(true);
        assert_eq!(
            cfg.to_client_context().application_intent,
            ApplicationIntent::ReadOnly
        );
        cfg.readonly(false);
        assert_eq!(
            cfg.to_client_context().application_intent,
            ApplicationIntent::ReadWrite
        );
    }

    #[test]
    fn instance_name_in_datasource() {
        let mut cfg = Config::new();
        cfg.host("server").instance_name("SQLEXPRESS");
        // Port is omitted when an instance name is set so mssql-tds runs SSRP.
        assert_eq!(cfg.datasource_string(), "tcp:server\\SQLEXPRESS");
    }

    #[test]
    fn instance_name_overrides_port_in_datasource() {
        let mut cfg = Config::new();
        cfg.host("server").port(14330).instance_name("SQLEXPRESS");
        // Even with an explicit port, instance triggers SSRP; port is dropped.
        assert_eq!(cfg.datasource_string(), "tcp:server\\SQLEXPRESS");
    }

    #[test]
    fn integrated_auth() {
        let mut cfg = Config::new();
        cfg.authentication(AuthMethod::integrated());
        let ctx = cfg.to_client_context();
        assert!(matches!(
            ctx.tds_authentication_method,
            TdsAuthenticationMethod::SSPI
        ));
    }

    #[test]
    fn aad_token_sets_access_token() {
        let token = uuid::Uuid::new_v4().to_string();
        let mut cfg = Config::new();
        cfg.authentication(AuthMethod::aad_token(&token));
        let ctx = cfg.to_client_context();
        assert!(matches!(
            ctx.tds_authentication_method,
            TdsAuthenticationMethod::AccessToken
        ));
        assert_eq!(ctx.access_token.as_deref(), Some(token.as_str()));
        // user_name/password must remain empty when using AAD
        assert!(ctx.user_name.is_empty());
        assert!(ctx.password.is_empty());
    }

    #[test]
    fn debug_redacts_authentication_secrets() {
        let secret = uuid::Uuid::new_v4().to_string();
        for auth in [
            AuthMethod::sql_server("sa", &secret),
            AuthMethod::aad_token(&secret),
        ] {
            let auth_debug = format!("{auth:?}");
            assert!(auth_debug.contains("<REDACTED>"));
            assert!(!auth_debug.contains(&secret));

            let mut cfg = Config::new();
            cfg.authentication(auth);
            let config_debug = format!("{cfg:?}");
            assert!(config_debug.contains("<REDACTED>"));
            assert!(!config_debug.contains(&secret));
        }
        assert_eq!(format!("{:?}", AuthMethod::Integrated), "Integrated");
    }

    #[test]
    fn driver_identity_is_set() {
        let cfg = Config::new();
        let ctx = cfg.to_client_context();

        // Login7 library name
        assert_eq!(ctx.library_name, "MS-MSSQL-RS");
        assert_eq!(
            ctx.driver_version.major.to_string(),
            env!("CARGO_PKG_VERSION_MAJOR")
        );
        assert_eq!(
            ctx.driver_version.minor.to_string(),
            env!("CARGO_PKG_VERSION_MINOR")
        );
        assert_eq!(
            ctx.driver_version.build.to_string(),
            env!("CARGO_PKG_VERSION_PATCH")
        );

        // UserAgent feature extension fields
        assert_eq!(ctx.user_agent.library_name, "MS-MSSQL-RS");
        assert_eq!(ctx.user_agent.driver_version, env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn client_name_sets_workstation_id() {
        let mut cfg = Config::new();
        cfg.client_name("app-host-01");
        assert_eq!(cfg.to_client_context().workstation_id, "app-host-01");
    }

    #[test]
    fn default_client_name_comes_from_mssql_tds() {
        let cfg = Config::new();
        assert_eq!(
            cfg.to_client_context().workstation_id,
            ClientContext::default().workstation_id
        );
    }

    #[test]
    fn host_name_in_certificate_sets_tls_override() {
        let mut cfg = Config::new();
        cfg.host_name_in_certificate("sql.example.com");
        assert_eq!(
            cfg.to_client_context()
                .encryption_options
                .host_name_in_cert
                .as_deref(),
            Some("sql.example.com")
        );
    }

    #[test]
    fn send_string_parameters_as_unicode_defaults_to_true() {
        let mut cfg = Config::new();
        assert!(cfg.string_parameters_as_unicode());
        cfg.send_string_parameters_as_unicode(false);
        assert!(!cfg.string_parameters_as_unicode());
    }

    #[test]
    fn encryption_level_strict_maps_to_strict() {
        use mssql_tds::core::EncryptionSetting;
        let mut cfg = Config::new();
        cfg.encryption(EncryptionLevel::Strict);
        assert_eq!(
            cfg.to_client_context().encryption_options.mode,
            EncryptionSetting::Strict
        );
    }

    #[test]
    fn encryption_level_required_maps_to_required() {
        use mssql_tds::core::EncryptionSetting;
        let mut cfg = Config::new();
        cfg.encryption(EncryptionLevel::Required);
        assert_eq!(
            cfg.to_client_context().encryption_options.mode,
            EncryptionSetting::Required
        );
    }

    #[test]
    fn encryption_level_on_maps_to_on() {
        use mssql_tds::core::EncryptionSetting;
        let mut cfg = Config::new();
        cfg.encryption(EncryptionLevel::On);
        assert_eq!(
            cfg.to_client_context().encryption_options.mode,
            EncryptionSetting::On
        );
    }

    #[test]
    fn encryption_level_off_maps_to_prefer_off() {
        use mssql_tds::core::EncryptionSetting;
        let mut cfg = Config::new();
        cfg.encryption(EncryptionLevel::Off);
        assert_eq!(
            cfg.to_client_context().encryption_options.mode,
            EncryptionSetting::PreferOff
        );
    }

    #[test]
    fn encryption_level_not_supported_maps_to_prefer_off() {
        use mssql_tds::core::EncryptionSetting;
        let mut cfg = Config::new();
        cfg.encryption(EncryptionLevel::NotSupported);
        assert_eq!(
            cfg.to_client_context().encryption_options.mode,
            EncryptionSetting::PreferOff
        );
    }

    #[test]
    fn strict_preserves_host_name_in_certificate_and_ca() {
        use std::path::Path;

        // Strict mode must still honour SNI override and custom CA bundle —
        // only `trust_cert` (TrustServerCertificate) is ignored by mssql-tds
        // under Strict.
        let mut cfg = Config::new();
        cfg.encryption(EncryptionLevel::Strict)
            .host_name_in_certificate("sql.example.com")
            .trust_cert_ca("/etc/ssl/ca.pem");
        let opts = cfg.to_client_context().encryption_options;
        assert_eq!(opts.mode, mssql_tds::core::EncryptionSetting::Strict);
        assert_eq!(opts.host_name_in_cert.as_deref(), Some("sql.example.com"));
        assert_eq!(
            opts.server_certificate.as_deref(),
            Some(Path::new("/etc/ssl/ca.pem"))
        );
    }

    #[test]
    fn multi_subnet_failover_defaults_to_off() {
        let cfg = Config::new();
        assert!(!cfg.is_multi_subnet_failover());
        assert_eq!(cfg.to_client_context().multi_subnet_failover, Some(false));
    }

    #[test]
    fn multi_subnet_failover_setter_propagates() {
        let mut cfg = Config::new();
        cfg.multi_subnet_failover(true);
        assert!(cfg.is_multi_subnet_failover());
        assert_eq!(cfg.to_client_context().multi_subnet_failover, Some(true));

        cfg.multi_subnet_failover(false);
        assert!(!cfg.is_multi_subnet_failover());
        assert_eq!(cfg.to_client_context().multi_subnet_failover, Some(false));
    }

    #[test]
    fn transport_defaults_to_tcp() {
        let cfg = Config::new();
        assert_eq!(cfg.get_transport(), &Transport::Tcp);
        assert_eq!(cfg.datasource_string(), "tcp:localhost,1433");
    }

    #[test]
    fn named_pipe_with_explicit_path_uses_supplied_pipe() {
        let mut cfg = Config::new();
        cfg.named_pipe(Some("\\\\.\\pipe\\sql\\query"));
        assert_eq!(
            cfg.get_transport(),
            &Transport::NamedPipe {
                pipe: Some("\\\\.\\pipe\\sql\\query".to_string()),
            }
        );
        assert_eq!(cfg.datasource_string(), "np:\\\\.\\pipe\\sql\\query");
    }

    #[test]
    fn named_pipe_default_path_for_default_instance_uses_local_dot() {
        let mut cfg = Config::new();
        cfg.host("localhost").named_pipe(None::<String>);
        assert_eq!(cfg.datasource_string(), "np:\\\\.\\pipe\\sql\\query");
    }

    #[test]
    fn named_pipe_default_path_for_remote_host_uses_host() {
        let mut cfg = Config::new();
        cfg.host("dbhost.lab").named_pipe(None::<String>);
        assert_eq!(
            cfg.datasource_string(),
            "np:\\\\dbhost.lab\\pipe\\sql\\query"
        );
    }

    #[test]
    fn named_pipe_default_path_for_named_instance() {
        let mut cfg = Config::new();
        cfg.host("dbhost.lab")
            .instance_name("SQLEXPRESS")
            .named_pipe(None::<String>);
        assert_eq!(
            cfg.datasource_string(),
            "np:\\\\dbhost.lab\\pipe\\MSSQL$SQLEXPRESS\\sql\\query"
        );
    }

    #[test]
    fn shared_memory_default_instance() {
        let mut cfg = Config::new();
        cfg.host(".").shared_memory();
        assert_eq!(cfg.get_transport(), &Transport::SharedMemory);
        assert_eq!(cfg.datasource_string(), "lpc:.");
    }

    #[test]
    fn shared_memory_named_instance() {
        let mut cfg = Config::new();
        cfg.host(".").instance_name("SQLEXPRESS").shared_memory();
        assert_eq!(cfg.datasource_string(), "lpc:.\\SQLEXPRESS");
    }

    #[test]
    fn transport_setter_round_trips() {
        let mut cfg = Config::new();
        cfg.transport(Transport::SharedMemory);
        assert_eq!(cfg.get_transport(), &Transport::SharedMemory);
        cfg.transport(Transport::Tcp);
        assert_eq!(cfg.get_transport(), &Transport::Tcp);
    }
}
