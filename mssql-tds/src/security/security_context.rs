// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Abstract security context trait for integrated authentication.

/// Result of generating an authentication token.
#[derive(Debug, Clone)]
pub struct SspiAuthToken {
    /// The token data to send to the server.
    pub data: Vec<u8>,

    /// Whether authentication is complete.
    ///
    /// - `true`: No more rounds needed, authentication succeeded.
    /// - `false`: Server will send a challenge, call `generate_token` again with the challenge.
    pub is_complete: bool,
}

impl SspiAuthToken {
    /// Creates a new auth token.
    pub fn new(data: Vec<u8>, is_complete: bool) -> Self {
        Self { data, is_complete }
    }

    /// Creates an empty token indicating completion.
    pub fn complete() -> Self {
        Self {
            data: Vec::new(),
            is_complete: true,
        }
    }

    /// Creates a token with data, indicating more rounds needed.
    pub fn continue_needed(data: Vec<u8>) -> Self {
        Self {
            data,
            is_complete: false,
        }
    }
}

/// Security package selection for Windows SSPI.
///
/// On Linux/macOS, only Kerberos is supported via GSSAPI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SecurityPackage {
    /// Negotiate package (default).
    ///
    /// Automatically selects between Kerberos and NTLM based on availability.
    /// This is the recommended setting for most scenarios.
    #[default]
    Negotiate,

    /// Kerberos package only.
    ///
    /// Requires a domain environment with a KDC (Key Distribution Center).
    /// Will fail if Kerberos is not available.
    Kerberos,

    /// NTLM package only (Windows only).
    ///
    /// Uses NT LAN Manager authentication. Works in non-domain environments
    /// but provides weaker security than Kerberos.
    Ntlm,
}

impl SecurityPackage {
    /// Returns the SSPI package name string.
    #[cfg(windows)]
    pub fn as_sspi_name(&self) -> &'static str {
        match self {
            SecurityPackage::Negotiate => "Negotiate",
            SecurityPackage::Kerberos => "Kerberos",
            SecurityPackage::Ntlm => "NTLM",
        }
    }

    /// Returns the display name for this package.
    pub fn display_name(&self) -> &'static str {
        match self {
            SecurityPackage::Negotiate => "Negotiate",
            SecurityPackage::Kerberos => "Kerberos",
            SecurityPackage::Ntlm => "NTLM",
        }
    }
}

/// Configuration for integrated authentication.
#[derive(Debug, Clone, Default)]
pub struct IntegratedAuthConfig {
    /// Explicit SPN (Service Principal Name) for the server.
    ///
    /// If `None`, the SPN will be auto-generated as `MSSQLSvc/<server>:<port>`.
    /// Set this if the SQL Server uses a non-standard SPN or if auto-generation fails.
    pub server_spn: Option<String>,

    /// Security package to use (Windows only).
    ///
    /// On Linux/macOS, Kerberos is always used regardless of this setting.
    pub security_package: SecurityPackage,

    /// Channel bindings data for extended protection.
    ///
    /// This is derived from the TLS connection and provides additional
    /// security against man-in-the-middle attacks.
    pub channel_bindings: Option<Vec<u8>>,

    /// Whether this is a loopback connection (localhost).
    ///
    /// On Windows, loopback connections may retry with an empty SPN
    /// to allow NTLM fallback.
    pub is_loopback: bool,
}

impl IntegratedAuthConfig {
    /// Creates a new configuration with default settings.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates a configuration with an explicit SPN.
    pub fn with_spn(spn: String) -> Self {
        Self {
            server_spn: Some(spn),
            ..Default::default()
        }
    }

    /// Sets the security package to use.
    pub fn with_package(mut self, package: SecurityPackage) -> Self {
        self.security_package = package;
        self
    }

    /// Sets channel bindings for extended protection.
    pub fn with_channel_bindings(mut self, bindings: Vec<u8>) -> Self {
        self.channel_bindings = Some(bindings);
        self
    }

    /// Marks this as a loopback connection.
    pub fn with_loopback(mut self, is_loopback: bool) -> Self {
        self.is_loopback = is_loopback;
        self
    }
}

/// Abstract trait for platform-specific security contexts.
///
/// This trait defines the interface for SSPI (Windows) and GSSAPI (Linux/macOS)
/// authentication. Implementations handle the platform-specific details of
/// acquiring credentials and generating authentication tokens.
///
/// # Authentication Flow
///
/// 1. Create a security context with `new()`
/// 2. Call `generate_token(None)` to get the initial token
/// 3. Send the token to the server in the Login7 packet
/// 4. If `is_complete()` is false, receive the server's challenge
/// 5. Call `generate_token(Some(challenge))` with the challenge
/// 6. Send the response token
/// 7. Repeat steps 4-6 until `is_complete()` returns true
///
/// # Example
///
/// ```ignore
/// use mssql_tds::security::{SecurityContext, IntegratedAuthConfig, create_security_context};
///
/// let config = IntegratedAuthConfig::new();
/// let mut ctx = create_security_context(&config, "server.contoso.com", 1433)?;
///
/// // Generate initial token
/// let initial = ctx.generate_token(None)?;
/// send_login7_with_sspi(&initial.data);
///
/// // Handle challenge-response loop
/// while !ctx.is_complete() {
///     let challenge = receive_sspi_token();
///     let response = ctx.generate_token(Some(&challenge))?;
///     if !response.data.is_empty() {
///         send_sspi_message(&response.data);
///     }
/// }
/// ```
pub trait SecurityContext: Send + Sync {
    /// Returns the name of the security package being used.
    ///
    /// Examples: "Negotiate", "Kerberos", "NTLM"
    fn package_name(&self) -> &str;

    /// Generates the next authentication token.
    ///
    /// # Arguments
    ///
    /// * `server_token` - The challenge token from the server, or `None` for the initial call.
    ///
    /// # Returns
    ///
    /// An `SspiAuthToken` containing the token data and completion status.
    ///
    /// # Errors
    ///
    /// Returns `SecurityError` if token generation fails.
    fn generate_token(
        &mut self,
        server_token: Option<&[u8]>,
    ) -> Result<SspiAuthToken, super::SecurityError>;

    /// Returns whether the authentication is complete.
    fn is_complete(&self) -> bool;

    /// Returns the SPN (Service Principal Name) being used.
    fn spn(&self) -> &str;

    /// The service name as the platform's security library receives it. Differs
    /// from [`spn`](Self::spn) where the library takes another form, such as
    /// GSSAPI's `service@host`.
    fn imported_name(&self) -> &str {
        self.spn()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auth_token_new() {
        let token = SspiAuthToken::new(vec![1, 2, 3], false);
        assert_eq!(token.data, vec![1, 2, 3]);
        assert!(!token.is_complete);
    }

    #[test]
    fn test_auth_token_complete() {
        let token = SspiAuthToken::complete();
        assert!(token.data.is_empty());
        assert!(token.is_complete);
    }

    #[test]
    fn test_auth_token_continue_needed() {
        let token = SspiAuthToken::continue_needed(vec![4, 5, 6]);
        assert_eq!(token.data, vec![4, 5, 6]);
        assert!(!token.is_complete);
    }

    #[test]
    fn test_security_package_default() {
        let pkg = SecurityPackage::default();
        assert_eq!(pkg, SecurityPackage::Negotiate);
    }

    #[test]
    fn test_security_package_display_name() {
        assert_eq!(SecurityPackage::Negotiate.display_name(), "Negotiate");
        assert_eq!(SecurityPackage::Kerberos.display_name(), "Kerberos");
        assert_eq!(SecurityPackage::Ntlm.display_name(), "NTLM");
    }

    #[test]
    fn test_integrated_auth_config_default() {
        let config = IntegratedAuthConfig::new();
        assert!(config.server_spn.is_none());
        assert_eq!(config.security_package, SecurityPackage::Negotiate);
        assert!(config.channel_bindings.is_none());
        assert!(!config.is_loopback);
    }

    #[test]
    fn test_integrated_auth_config_with_spn() {
        let config = IntegratedAuthConfig::with_spn("MSSQLSvc/server:1433".to_string());
        assert_eq!(config.server_spn, Some("MSSQLSvc/server:1433".to_string()));
    }

    #[test]
    fn test_integrated_auth_config_builder() {
        let config = IntegratedAuthConfig::new()
            .with_package(SecurityPackage::Kerberos)
            .with_channel_bindings(vec![1, 2, 3])
            .with_loopback(true);

        assert_eq!(config.security_package, SecurityPackage::Kerberos);
        assert_eq!(config.channel_bindings, Some(vec![1, 2, 3]));
        assert!(config.is_loopback);
    }
}
