// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Rustls-based TLS handler for TDS connections.
//!
//! This module provides the same TLS functionality as `ssl_handler.rs` (native-tls)
//! but uses the `rustls` crate as the backend. It is enabled via the `rustls-backend`
//! feature flag.

use crate::connection::transport::certificate_validator;
use crate::connection::transport::network_transport::Stream;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error as RustlsError, SignatureScheme};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use tokio_rustls::TlsConnector;
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

/// Cache of pre-built `ClientConfig` instances keyed by validation config.
static CONFIG_CACHE: std::sync::LazyLock<RwLock<HashMap<TlsValidationConfig, Arc<ClientConfig>>>> =
    std::sync::LazyLock::new(|| RwLock::new(HashMap::new()));

/// A certificate verifier that accepts all certificates (dangerous, for trust_server_certificate).
#[derive(Debug)]
struct AcceptAllCertVerifier;

impl ServerCertVerifier for AcceptAllCertVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::RSA_PKCS1_SHA384,
            SignatureScheme::RSA_PKCS1_SHA512,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ECDSA_NISTP384_SHA384,
            SignatureScheme::ECDSA_NISTP521_SHA512,
            SignatureScheme::RSA_PSS_SHA256,
            SignatureScheme::RSA_PSS_SHA384,
            SignatureScheme::RSA_PSS_SHA512,
            SignatureScheme::ED25519,
            SignatureScheme::ED448,
        ]
    }
}

/// A certificate verifier that accepts any certificate but skips hostname validation.
/// Used for accept_invalid_hostnames scenarios without disabling cert chain validation.
#[derive(Debug)]
struct SkipHostnameVerifier {
    inner: Arc<rustls::client::WebPkiServerVerifier>,
}

impl ServerCertVerifier for SkipHostnameVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, RustlsError> {
        // Verify the cert chain but skip hostname verification by passing a
        // dummy name that will not be checked against the certificate's SANs.
        // We use "hostname.not" which is a syntactically-valid DNS name but
        // will never match any real certificate SAN. However, since
        // WebPkiServerVerifier validates both chain AND hostname, we cannot
        // use it directly. Instead, we call verify_server_cert and if it
        // fails only due to hostname mismatch, we still accept.
        let dummy_name =
            ServerName::try_from("hostname.verification.disabled").expect("valid DNS name");
        match self.inner.verify_server_cert(
            end_entity,
            intermediates,
            &dummy_name,
            ocsp_response,
            now,
        ) {
            Ok(verified) => Ok(verified),
            Err(RustlsError::InvalidCertificate(rustls::CertificateError::NotValidForName)) => {
                // Hostname mismatch is expected — we're intentionally skipping it.
                // The chain itself was validated successfully.
                Ok(ServerCertVerified::assertion())
            }
            Err(e) => Err(e),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, RustlsError> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

fn get_or_build_config(validation: &TlsValidationConfig) -> TdsResult<Arc<ClientConfig>> {
    // Ensure the ring crypto provider is installed (no-op if already set).
    let _ = rustls::crypto::ring::default_provider().install_default();

    if let Some(config) = CONFIG_CACHE
        .read()
        .map_err(|_| {
            crate::error::Error::ImplementationError(
                "TLS config cache read lock poisoned".to_string(),
            )
        })?
        .get(validation)
    {
        return Ok(Arc::clone(config));
    }

    let config = build_client_config(validation)?;
    let config = Arc::new(config);

    CONFIG_CACHE
        .write()
        .map_err(|_| {
            crate::error::Error::ImplementationError(
                "TLS config cache write lock poisoned".to_string(),
            )
        })?
        .insert(validation.clone(), Arc::clone(&config));
    Ok(config)
}

fn build_client_config(validation: &TlsValidationConfig) -> TdsResult<ClientConfig> {
    let mut config = if validation.accept_invalid_certs {
        ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAllCertVerifier))
            .with_no_client_auth()
    } else if validation.accept_invalid_hostnames {
        // Verify certs against root store but skip hostname verification
        let mut root_store = rustls::RootCertStore::empty();
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

        let native_certs_result = rustls_native_certs::load_native_certs();
        if !native_certs_result.errors.is_empty() {
            debug!(
                "Errors loading some native certs (using webpki-roots as fallback): {:?}",
                native_certs_result.errors
            );
        }
        for cert in native_certs_result.certs {
            if let Err(e) = root_store.add(cert) {
                debug!("Skipping a native cert that couldn't be added: {e}");
            }
        }

        let inner = rustls::client::WebPkiServerVerifier::builder(Arc::new(root_store))
            .build()
            .map_err(|e| crate::error::Error::RustlsIoError(Box::new(e)))?;
        let verifier = SkipHostnameVerifier { inner };
        ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(verifier))
            .with_no_client_auth()
    } else {
        // Build with system root certificates
        let mut root_store = rustls::RootCertStore::empty();

        // Add Mozilla's root certificates as a fallback
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

        // Also try to load native system certificates
        let native_certs_result = rustls_native_certs::load_native_certs();
        if !native_certs_result.errors.is_empty() {
            debug!(
                "Errors loading some native certs (using webpki-roots as fallback): {:?}",
                native_certs_result.errors
            );
        }
        for cert in native_certs_result.certs {
            if let Err(e) = root_store.add(cert) {
                debug!("Skipping a native cert that couldn't be added: {e}");
            }
        }

        ClientConfig::builder()
            .with_root_certificates(root_store)
            .with_no_client_auth()
    };

    if validation.use_alpn {
        config.alpn_protocols = vec![TDS_8_ALPN_PROTOCOL.as_bytes().to_vec()];
    }

    Ok(config)
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
            "TLS config (rustls): encryption_mode={:?}, trust_server_certificate={}, server_certificate={:?}, host_name_in_cert={:?}, resolved_host_name={}, server_host_name={}",
            self.encryption_options.mode,
            self.encryption_options.trust_server_certificate,
            self.encryption_options.server_certificate,
            self.encryption_options.host_name_in_cert,
            host_name,
            self.server_host_name,
        );

        let config = get_or_build_config(&validation)?;
        let connector = TlsConnector::from(config);

        // Parse the server name for rustls
        let server_name = ServerName::try_from(host_name.to_string()).map_err(|e| {
            crate::error::Error::ConnectionError(format!(
                "Invalid TLS server name '{}': {}",
                host_name, e
            ))
        })?;

        info!(
            "Starting TLS handshake (rustls) to {} using host {}",
            self.server_host_name, host_name
        );

        let encrypted_stream = connector.connect(server_name, base_stream).await;

        match encrypted_stream {
            Ok(mut stream) => {
                if validation.use_alpn {
                    match stream.get_ref().1.alpn_protocol() {
                        Some(proto) => {
                            debug!("Server negotiated ALPN: {}", String::from_utf8_lossy(proto));
                        }
                        None => {
                            debug!("Server did not negotiate an ALPN protocol");
                        }
                    }
                }

                // If ServerCertificate is specified, perform certificate pinning validation
                if let Some(cert_path) = &self.encryption_options.server_certificate {
                    info!("Validating server certificate using: {cert_path:?}");

                    // Get the server's certificate chain from the rustls connection
                    let peer_certs = stream
                        .get_ref()
                        .1
                        .peer_certificates()
                        .ok_or(crate::error::Error::NoServerCertificate)?;

                    let server_cert_der = peer_certs
                        .first()
                        .ok_or(crate::error::Error::NoServerCertificate)?;

                    // Validate the certificate
                    certificate_validator::validate_server_certificate(
                        cert_path,
                        server_cert_der.as_ref(),
                    )?;

                    info!("Server certificate validation successful");
                }

                // Call tls_handshake_completed on the underlying stream
                // For tokio-rustls, the structure is TlsStream -> (IO, ClientConnection)
                // where IO is our Box<dyn Stream>
                stream.get_mut().0.tls_handshake_completed();
                Ok(Box::new(stream))
            }
            Err(e) => {
                error!(
                    "TLS handshake FAILED (rustls): host_name={}, server_host_name={}, error={:?}, \
                     trust_server_certificate={}, encryption_mode={:?}, os={}",
                    host_name,
                    self.server_host_name,
                    e,
                    self.encryption_options.trust_server_certificate,
                    self.encryption_options.mode,
                    std::env::consts::OS,
                );
                Err(crate::error::Error::RustlsIoError(Box::new(e)))
            }
        }
    }
}

/// Implement Stream for tokio_rustls TlsStream
impl Stream for tokio_rustls::client::TlsStream<Box<dyn Stream>> {
    fn tls_handshake_starting(&mut self) {
        // tokio_rustls::client::TlsStream wraps (IO, ClientConnection)
        // Access IO via get_mut().0
        self.get_mut().0.tls_handshake_starting();
    }

    fn tls_handshake_completed(&mut self) {
        self.get_mut().0.tls_handshake_completed();
    }

    fn is_connection_dead(&self) -> bool {
        self.get_ref().0.is_connection_dead()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustls::pki_types::CertificateDer;

    /// Install the ring crypto provider for tests that call `ClientConfig::builder()`.
    /// This is a no-op if already installed (e.g., by another test in the same process).
    fn ensure_crypto_provider() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }

    fn default_options() -> EncryptionOptions {
        EncryptionOptions {
            mode: EncryptionSetting::Required,
            trust_server_certificate: false,
            host_name_in_cert: None,
            server_certificate: None,
        }
    }

    // ─── resolve_tls_validation tests (mirrors ssl_handler.rs) ───

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

    // ─── build_client_config tests (rustls-specific) ───

    #[test]
    fn build_config_accept_invalid_certs_does_not_panic() {
        ensure_crypto_provider();
        let validation = TlsValidationConfig {
            accept_invalid_certs: true,
            accept_invalid_hostnames: false,
            use_alpn: false,
        };
        let config = build_client_config(&validation).expect("should build config");
        // When accepting invalid certs, ALPN should not be set
        assert!(config.alpn_protocols.is_empty());
    }

    #[test]
    fn build_config_with_alpn_sets_tds8_protocol() {
        ensure_crypto_provider();
        let validation = TlsValidationConfig {
            accept_invalid_certs: true,
            accept_invalid_hostnames: false,
            use_alpn: true,
        };
        let config = build_client_config(&validation).expect("should build config");
        assert_eq!(config.alpn_protocols.len(), 1);
        assert_eq!(config.alpn_protocols[0], TDS_8_ALPN_PROTOCOL.as_bytes());
    }

    #[test]
    fn build_config_without_alpn_has_empty_protocols() {
        ensure_crypto_provider();
        let validation = TlsValidationConfig {
            accept_invalid_certs: false,
            accept_invalid_hostnames: false,
            use_alpn: false,
        };
        let config = build_client_config(&validation).expect("should build config");
        assert!(config.alpn_protocols.is_empty());
    }

    #[test]
    fn build_config_accept_invalid_hostnames_builds_successfully() {
        ensure_crypto_provider();
        let validation = TlsValidationConfig {
            accept_invalid_certs: false,
            accept_invalid_hostnames: true,
            use_alpn: false,
        };
        // Should not panic — uses SkipHostnameVerifier with WebPKI chain verification
        let config = build_client_config(&validation).expect("should build config");
        assert!(config.alpn_protocols.is_empty());
    }

    #[test]
    fn build_config_full_validation_loads_root_certs() {
        ensure_crypto_provider();
        let validation = TlsValidationConfig {
            accept_invalid_certs: false,
            accept_invalid_hostnames: false,
            use_alpn: false,
        };
        // Full validation path: loads webpki-roots + native certs
        let _config =
            build_client_config(&validation).expect("should build config with root certs");
    }

    // ─── Config caching tests ───

    #[test]
    fn config_cache_returns_same_arc_for_same_validation() {
        ensure_crypto_provider();
        let validation = TlsValidationConfig {
            accept_invalid_certs: true,
            accept_invalid_hostnames: true,
            use_alpn: true,
        };
        let config1 = get_or_build_config(&validation).expect("should get config");
        let config2 = get_or_build_config(&validation).expect("should get config again");
        assert!(
            Arc::ptr_eq(&config1, &config2),
            "cache should return the same Arc"
        );
    }

    #[test]
    fn config_cache_returns_different_arc_for_different_validation() {
        ensure_crypto_provider();
        let v1 = TlsValidationConfig {
            accept_invalid_certs: true,
            accept_invalid_hostnames: false,
            use_alpn: false,
        };
        let v2 = TlsValidationConfig {
            accept_invalid_certs: false,
            accept_invalid_hostnames: false,
            use_alpn: true,
        };
        let config1 = get_or_build_config(&v1).expect("should get config1");
        let config2 = get_or_build_config(&v2).expect("should get config2");
        assert!(
            !Arc::ptr_eq(&config1, &config2),
            "different configs should be different Arcs"
        );
    }

    // ─── AcceptAllCertVerifier tests ───

    #[test]
    fn accept_all_verifier_accepts_any_cert() {
        let verifier = AcceptAllCertVerifier;
        // Create a dummy self-signed-like DER cert (just random bytes — verifier should not care)
        let dummy_cert = CertificateDer::from(vec![0u8; 128]);
        let server_name = ServerName::try_from("example.com").unwrap();
        let result =
            verifier.verify_server_cert(&dummy_cert, &[], &server_name, &[], UnixTime::now());
        assert!(
            result.is_ok(),
            "AcceptAllCertVerifier should accept any certificate"
        );
    }

    #[test]
    fn accept_all_verifier_returns_supported_schemes() {
        let verifier = AcceptAllCertVerifier;
        let schemes = verifier.supported_verify_schemes();
        assert!(
            !schemes.is_empty(),
            "should report supported signature schemes"
        );
    }

    // ─── TlsValidationConfig equality / hashing tests ───

    #[test]
    fn tls_validation_config_eq_and_hash() {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let v1 = TlsValidationConfig {
            accept_invalid_certs: true,
            accept_invalid_hostnames: false,
            use_alpn: true,
        };
        let v2 = TlsValidationConfig {
            accept_invalid_certs: true,
            accept_invalid_hostnames: false,
            use_alpn: true,
        };
        assert_eq!(v1, v2);

        let mut h1 = DefaultHasher::new();
        let mut h2 = DefaultHasher::new();
        v1.hash(&mut h1);
        v2.hash(&mut h2);
        assert_eq!(h1.finish(), h2.finish());
    }

    #[test]
    fn tls_validation_config_ne_when_fields_differ() {
        let v1 = TlsValidationConfig {
            accept_invalid_certs: true,
            accept_invalid_hostnames: false,
            use_alpn: false,
        };
        let v2 = TlsValidationConfig {
            accept_invalid_certs: false,
            accept_invalid_hostnames: false,
            use_alpn: false,
        };
        assert_ne!(v1, v2);
    }
}
