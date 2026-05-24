// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::connection::transport::certificate_validator;
use crate::connection::transport::network_transport::Stream;
use native_tls::TlsConnector as NativeTlsConnector;
use std::collections::HashMap;
use std::sync::RwLock;
use tokio_native_tls::TlsStream;
use tracing::{debug, error, info, warn};

use crate::core::{
    EncryptionOptions, EncryptionSetting, NegotiatedEncryptionSetting, TDS_8_ALPN_PROTOCOL,
    TdsResult,
};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct TlsValidationConfig {
    pub accept_invalid_certs: bool,
    pub accept_invalid_hostnames: bool,
    pub use_alpn: bool,
}

/// Cache of pre-built `NativeTlsConnector` instances keyed by validation config.
/// Building a connector is expensive (~50ms on Linux) because `native-tls` loads
/// and parses the system CA certificate store via OpenSSL on every call to
/// `builder().build()`. Caching avoids this cost on subsequent connections.
static CONNECTOR_CACHE: std::sync::LazyLock<
    RwLock<HashMap<TlsValidationConfig, NativeTlsConnector>>,
> = std::sync::LazyLock::new(|| RwLock::new(HashMap::new()));

fn get_or_build_connector(validation: &TlsValidationConfig) -> TdsResult<NativeTlsConnector> {
    if let Some(connector) = CONNECTOR_CACHE
        .read()
        .map_err(|_| {
            crate::error::Error::ImplementationError(
                "TLS connector cache read lock poisoned".to_string(),
            )
        })?
        .get(validation)
    {
        return Ok(connector.clone());
    }

    let mut builder = NativeTlsConnector::builder();
    if validation.accept_invalid_certs {
        builder.danger_accept_invalid_certs(true);
    }
    if validation.accept_invalid_hostnames {
        builder.danger_accept_invalid_hostnames(true);
    }
    if validation.use_alpn {
        builder.request_alpns(&[TDS_8_ALPN_PROTOCOL]);
    }
    let connector = builder.build()?;

    CONNECTOR_CACHE
        .write()
        .map_err(|_| {
            crate::error::Error::ImplementationError(
                "TLS connector cache write lock poisoned".to_string(),
            )
        })?
        .insert(validation.clone(), connector.clone());
    Ok(connector)
}

#[derive(Debug)]
pub(crate) struct SslHandler {
    pub(crate) server_host_name: String,
    pub(crate) encryption_options: EncryptionOptions,
}

impl SslHandler {
    /// Determine TLS certificate validation behavior based on encryption options
    /// and the negotiated encryption mode.
    pub(crate) fn resolve_tls_validation(
        encryption_options: &EncryptionOptions,
        negotiated_encryption: NegotiatedEncryptionSetting,
    ) -> TlsValidationConfig {
        let use_alpn = negotiated_encryption == NegotiatedEncryptionSetting::Strict;

        if encryption_options.server_certificate.is_some() {
            // Certificate pinning mode: bypass CA validation, custom check later
            TlsValidationConfig {
                accept_invalid_certs: true,
                accept_invalid_hostnames: true,
                use_alpn,
            }
        } else if negotiated_encryption == NegotiatedEncryptionSetting::LoginOnly {
            // ODBC parity: LoginOnly skips cert validation unconditionally
            TlsValidationConfig {
                accept_invalid_certs: true,
                accept_invalid_hostnames: false,
                use_alpn,
            }
        } else if encryption_options.trust_server_certificate
            && encryption_options.mode != EncryptionSetting::Strict
        {
            TlsValidationConfig {
                accept_invalid_certs: true,
                accept_invalid_hostnames: false,
                use_alpn,
            }
        } else {
            TlsValidationConfig {
                accept_invalid_certs: false,
                accept_invalid_hostnames: false,
                use_alpn,
            }
        }
    }

    pub(crate) async fn enable_ssl_async(
        &self,
        mut base_stream: Box<dyn Stream>,
        negotiated_encryption: NegotiatedEncryptionSetting,
    ) -> TdsResult<Box<dyn Stream>> {
        base_stream.tls_handshake_starting();

        // Check if ServerCertificate and TrustServerCertificate are both specified
        if self.encryption_options.server_certificate.is_some()
            && self.encryption_options.trust_server_certificate
        {
            warn!(
                "Both ServerCertificate and TrustServerCertificate are specified. ServerCertificate takes precedence."
            );
        }

        // Check if ServerCertificate and HostnameInCertificate are both specified
        if self.encryption_options.server_certificate.is_some()
            && self.encryption_options.host_name_in_cert.is_some()
        {
            return Err(crate::error::Error::UsageError(
                "ServerCertificate and HostnameInCertificate are mutually exclusive. Use only one."
                    .to_string(),
            ));
        }

        // Log TrustServerCertificate being ignored in Strict mode
        if self.encryption_options.trust_server_certificate
            && self.encryption_options.mode == EncryptionSetting::Strict
        {
            warn!(
                "TrustServerCertificate is ignored for Strict encryption mode. Certificate validation will be enforced."
            );
        }

        let validation =
            Self::resolve_tls_validation(&self.encryption_options, negotiated_encryption);

        let host_name = self
            .encryption_options
            .host_name_in_cert
            .as_ref()
            .map_or_else(
                || &self.server_host_name,
                |host_name| {
                    if host_name.is_empty() {
                        &self.server_host_name
                    } else {
                        host_name
                    }
                },
            );

        info!(
            "TLS config: encryption_mode={:?}, trust_server_certificate={}, server_certificate={:?}, host_name_in_cert={:?}, resolved_host_name={}, server_host_name={}",
            self.encryption_options.mode,
            self.encryption_options.trust_server_certificate,
            self.encryption_options.server_certificate,
            self.encryption_options.host_name_in_cert,
            host_name,
            self.server_host_name,
        );

        let connector = get_or_build_connector(&validation)?;

        info!(
            "Starting TLS handshake to {} using host {}",
            self.server_host_name, host_name
        );
        let encrypted_stream = tokio_native_tls::TlsConnector::from(connector)
            .connect(host_name, base_stream)
            .await;

        match encrypted_stream {
            Ok(mut stream) => {
                if validation.use_alpn {
                    match stream.get_ref().negotiated_alpn() {
                        Ok(Some(ref proto)) => {
                            debug!("Server negotiated ALPN: {}", String::from_utf8_lossy(proto));
                        }
                        Ok(None) => {
                            debug!("Server did not negotiate an ALPN protocol");
                        }
                        Err(e) => {
                            debug!("Failed to query negotiated ALPN: {}", e);
                        }
                    }
                }

                // If ServerCertificate is specified, perform certificate validation
                if let Some(cert_path) = &self.encryption_options.server_certificate {
                    info!("Validating server certificate using: {cert_path:?}",);

                    // Get the server's certificate from the TLS stream
                    let peer_cert = stream
                        .get_ref()
                        .peer_certificate()
                        .map_err(crate::error::Error::TlsError)?
                        .ok_or(crate::error::Error::NoServerCertificate)?;

                    // Get the DER-encoded certificate data
                    let server_cert_der =
                        peer_cert.to_der().map_err(crate::error::Error::TlsError)?;

                    // Validate the certificate
                    certificate_validator::validate_server_certificate(
                        cert_path,
                        &server_cert_der,
                    )?;

                    info!("Server certificate validation successful");
                }

                // Call tls_handshake_completed on the underlying stream through the TlsStream wrapper
                stream
                    .get_mut()
                    .get_mut()
                    .get_mut()
                    .tls_handshake_completed();
                Ok(Box::new(stream))
            }
            Err(e) => {
                error!(
                    "TLS handshake FAILED: host_name={}, server_host_name={}, error={:?}, \
                     trust_server_certificate={}, encryption_mode={:?}, os={}",
                    host_name,
                    self.server_host_name,
                    e,
                    self.encryption_options.trust_server_certificate,
                    self.encryption_options.mode,
                    std::env::consts::OS,
                );
                // Note: We can't retrieve the certificate SANs from a failed handshake
                // because the connection is terminated before we can access the peer cert
                Err(crate::error::Error::TlsHandshakeError {
                    source: e,
                    expected_host: host_name.to_string(),
                    cert_sans:
                        "(unavailable - handshake failed before certificate could be retrieved)"
                            .to_string(),
                })
            }
        }
    }
}

impl Stream for TlsStream<Box<dyn Stream>> {
    fn tls_handshake_starting(&mut self) {
        // TlsStream wraps: tokio_native_tls::TlsStream -> native_tls::TlsStream -> AllowStd -> Box<dyn Stream>
        // So we need get_mut() three times to reach the underlying Box<dyn Stream>
        self.get_mut().get_mut().get_mut().tls_handshake_starting();
    }

    fn tls_handshake_completed(&mut self) {
        // TlsStream wraps: tokio_native_tls::TlsStream -> native_tls::TlsStream -> AllowStd -> Box<dyn Stream>
        // So we need get_mut() three times to reach the underlying Box<dyn Stream>
        self.get_mut().get_mut().get_mut().tls_handshake_completed();
    }

    fn is_connection_dead(&self) -> bool {
        // Navigate through the TLS wrapper chain using get_ref() to reach the underlying stream
        self.get_ref().get_ref().get_ref().is_connection_dead()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_options() -> EncryptionOptions {
        EncryptionOptions {
            mode: EncryptionSetting::Required,
            trust_server_certificate: false,
            host_name_in_cert: None,
            server_certificate: None,
        }
    }

    #[test]
    fn login_only_skips_cert_validation() {
        let opts = default_options();
        let config =
            SslHandler::resolve_tls_validation(&opts, NegotiatedEncryptionSetting::LoginOnly);
        assert!(config.accept_invalid_certs);
        assert!(!config.accept_invalid_hostnames);
        assert!(!config.use_alpn);
    }

    #[test]
    fn login_only_skips_cert_validation_even_with_trust_false() {
        let mut opts = default_options();
        opts.trust_server_certificate = false;
        opts.mode = EncryptionSetting::PreferOff;
        let config =
            SslHandler::resolve_tls_validation(&opts, NegotiatedEncryptionSetting::LoginOnly);
        assert!(config.accept_invalid_certs);
        assert!(!config.use_alpn);
    }

    #[test]
    fn mandatory_without_trust_enforces_validation() {
        let opts = default_options();
        let config =
            SslHandler::resolve_tls_validation(&opts, NegotiatedEncryptionSetting::Mandatory);
        assert!(!config.accept_invalid_certs);
        assert!(!config.accept_invalid_hostnames);
        assert!(!config.use_alpn);
    }

    #[test]
    fn mandatory_with_trust_skips_cert_validation() {
        let mut opts = default_options();
        opts.trust_server_certificate = true;
        let config =
            SslHandler::resolve_tls_validation(&opts, NegotiatedEncryptionSetting::Mandatory);
        assert!(config.accept_invalid_certs);
        assert!(!config.accept_invalid_hostnames);
        assert!(!config.use_alpn);
    }

    #[test]
    fn strict_ignores_trust_server_certificate() {
        let mut opts = default_options();
        opts.mode = EncryptionSetting::Strict;
        opts.trust_server_certificate = true;
        let config = SslHandler::resolve_tls_validation(&opts, NegotiatedEncryptionSetting::Strict);
        assert!(!config.accept_invalid_certs);
        assert!(!config.accept_invalid_hostnames);
        assert!(config.use_alpn);
    }

    #[test]
    fn server_certificate_enables_pinning_mode() {
        let mut opts = default_options();
        opts.server_certificate = Some("cert.pem".into());
        let config =
            SslHandler::resolve_tls_validation(&opts, NegotiatedEncryptionSetting::Mandatory);
        assert!(config.accept_invalid_certs);
        assert!(config.accept_invalid_hostnames);
        assert!(!config.use_alpn);
    }

    #[test]
    fn server_certificate_takes_precedence_over_login_only() {
        let mut opts = default_options();
        opts.server_certificate = Some("cert.pem".into());
        let config =
            SslHandler::resolve_tls_validation(&opts, NegotiatedEncryptionSetting::LoginOnly);
        assert!(config.accept_invalid_certs);
        assert!(config.accept_invalid_hostnames);
        assert!(!config.use_alpn);
    }

    #[test]
    fn no_encryption_enforces_validation() {
        let opts = default_options();
        let config =
            SslHandler::resolve_tls_validation(&opts, NegotiatedEncryptionSetting::NoEncryption);
        assert!(!config.accept_invalid_certs);
        assert!(!config.accept_invalid_hostnames);
        assert!(!config.use_alpn);
    }

    #[test]
    fn strict_enables_alpn() {
        let opts = default_options();
        let config = SslHandler::resolve_tls_validation(&opts, NegotiatedEncryptionSetting::Strict);
        assert!(config.use_alpn);
    }

    #[test]
    fn non_strict_modes_disable_alpn() {
        let opts = default_options();
        for mode in [
            NegotiatedEncryptionSetting::Mandatory,
            NegotiatedEncryptionSetting::LoginOnly,
            NegotiatedEncryptionSetting::NoEncryption,
        ] {
            let config = SslHandler::resolve_tls_validation(&opts, mode);
            assert!(!config.use_alpn, "use_alpn should be false for {:?}", mode);
        }
    }

    #[test]
    fn strict_with_server_certificate_enables_alpn() {
        let mut opts = default_options();
        opts.server_certificate = Some("cert.pem".into());
        let config = SslHandler::resolve_tls_validation(&opts, NegotiatedEncryptionSetting::Strict);
        assert!(config.use_alpn);
        assert!(config.accept_invalid_certs);
        assert!(config.accept_invalid_hostnames);
    }
}
