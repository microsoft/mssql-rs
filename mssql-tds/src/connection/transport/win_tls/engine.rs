// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The Schannel-direct [`TlsEngine`] implementation.
//!
//! Stitches together [`super::cred`], [`super::handshake`],
//! [`super::stream::SchannelTlsStream`], and [`super::validate`] into a
//! drop-in replacement for the native-tls engine.
//!
//! Enabled by default on Windows via the `tls-schannel-direct` Cargo
//! feature; [`super::super::tls::default_engine`] routes to it.

use async_trait::async_trait;
use std::sync::Arc;
use tracing::{error, info};

use super::alpn;
use super::cred::{self, CredKind};
use super::stream::SchannelTlsStream;
use super::validate;
use crate::connection::transport::network_transport::Stream;
use crate::connection::transport::tls::{TlsConnectParams, TlsEngine, TlsValidationConfig};
use crate::core::{TDS_8_ALPN_PROTOCOL, TdsResult};

/// Zero-sized engine type. Singleton accessed via [`SCHANNEL_ENGINE`].
#[derive(Debug)]
pub(crate) struct SchannelEngine;

pub(crate) static SCHANNEL_ENGINE: SchannelEngine = SchannelEngine;

/// Map user-facing validation config to the SChannel cred bucket.
///
/// Routing matches ODBC's three-bucket partitioning
/// (`SNI_SslProvider.cpp:1818-1821`): `ServerCertificate=<path>` (file-pin
/// mode) lands in [`CredKind::ManualValidate`] so its sessions are cached
/// separately from the `TrustServerCertificate=Yes` / `LoginOnly` traffic
/// that goes to [`CredKind::NoValidate`]. Both buckets skip Schannel's
/// chain build via the per-call `ISC_REQ_MANUAL_CRED_VALIDATION` bit; the
/// post-handshake DER compare for the pinned cert runs in
/// [`super::validate::validate_after_handshake`].
fn pick_cred_kind(validation: &TlsValidationConfig, has_pinned_cert: bool) -> CredKind {
    if has_pinned_cert || validation.server_ca_path.is_some() {
        // File pins and custom CA chains are validated after the handshake.
        CredKind::ManualValidate
    } else if validation.accept_invalid_certs {
        // TrustServerCertificate=Yes / LoginOnly: bypass chain build.
        CredKind::NoValidate
    } else {
        // Encrypt=Mandatory / Strict default: full SChannel chain +
        // hostname check inline during ISC.
        CredKind::AutoValidate
    }
}

#[async_trait]
impl TlsEngine for SchannelEngine {
    async fn connect(
        &self,
        mut base_stream: Box<dyn Stream>,
        params: TlsConnectParams<'_>,
    ) -> TdsResult<Box<dyn Stream>> {
        base_stream.tls_handshake_starting();

        let kind = pick_cred_kind(params.validation, params.server_certificate_path.is_some());
        let custom_roots: Option<Vec<Vec<u8>>> = params
            .validation
            .server_ca_path
            .as_deref()
            .map(super::super::certificate_validator::load_ca_certificates_from_file)
            .transpose()?
            .map(|certs| certs.iter().map(native_tls::Certificate::to_der).collect())
            .transpose()?;
        // Custom trust roots are reloaded and get a fresh TLS session cache
        // partition, so neither certificate rotation nor policy changes can
        // reuse a session authenticated with stale roots.
        let cred = if custom_roots.is_some() {
            cred::acquire_client_cred(kind).map(Arc::new)
        } else {
            cred::get_or_acquire(kind)
        }
        .map_err(|e| {
            crate::error::Error::ImplementationError(format!(
                "Schannel AcquireCredentialsHandle failed: {e}"
            ))
        })?;

        info!(
            "Starting Schannel TLS handshake to {} using host {} (kind={:?}, alpn={})",
            params.server_host_name, params.host_name, kind, params.validation.use_alpn,
        );

        let alpn_blob = if params.validation.use_alpn {
            Some(alpn::build_alpn_buffer(&[TDS_8_ALPN_PROTOCOL]))
        } else {
            None
        };

        let stream_result =
            SchannelTlsStream::connect(base_stream, cred, kind, params.host_name, alpn_blob).await;

        let mut stream = match stream_result {
            Ok(s) => s,
            Err(e) => {
                error!(
                    "Schannel TLS handshake FAILED: host_name={}, server_host_name={}, error={:?}",
                    params.host_name, params.server_host_name, e,
                );
                return Err(crate::error::Error::ImplementationError(format!(
                    "Schannel TLS handshake failed: {e}"
                )));
            }
        };

        if let Some(roots) = custom_roots {
            super::custom_ca::validate(stream.ctx(), &roots, params.host_name).map_err(|e| {
                crate::error::Error::ImplementationError(format!(
                    "Schannel custom CA validation failed: {e}"
                ))
            })?;
        } else if let Err(e) = validate::validate_after_handshake(
            stream.ctx(),
            kind,
            params.server_certificate_path.map(|p| p.as_path()),
        ) {
            error!(
                "Schannel post-handshake validation FAILED: host={}, error={}",
                params.host_name, e
            );
            return Err(e.into());
        }

        stream.get_mut().tls_handshake_completed();
        Ok(Box::new(stream))
    }
}

// AsyncRead/AsyncWrite for SchannelTlsStream<Box<dyn Stream>> are
// provided by the generic impl in stream.rs (where S: AsyncRead +
// AsyncWrite + Unpin). Box<dyn Stream> satisfies those bounds because
// `Stream: AsyncRead + AsyncWrite + Unpin + Send + Sync`.
impl Stream for SchannelTlsStream<Box<dyn Stream>> {
    fn tls_handshake_starting(&mut self) {
        self.get_mut().tls_handshake_starting();
    }

    fn tls_handshake_completed(&mut self) {
        self.get_mut().tls_handshake_completed();
    }

    fn is_connection_dead(&self) -> bool {
        self.get_ref().is_connection_dead()
    }

    fn channel_binding_token(&self) -> Option<Vec<u8>> {
        SchannelTlsStream::channel_binding_token(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cred_kind_for_custom_ca_is_manual() {
        let validation = TlsValidationConfig {
            accept_invalid_certs: false,
            accept_invalid_hostnames: false,
            use_alpn: false,
            server_ca_path: Some("ca.pem".into()),
        };
        assert_eq!(pick_cred_kind(&validation, false), CredKind::ManualValidate);
    }

    #[tokio::test]
    async fn custom_ca_default_engine_retains_channel_bindings() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let identity_path = "tests/test_certificates/ca_signed_identity.pfx";
        let identity_bytes = std::fs::read(identity_path)
            .expect("generate fixtures with scripts/generate_mock_tds_server_certs.ps1");
        let identity = native_tls::Identity::from_pkcs12(&identity_bytes, "").unwrap();
        let acceptor = native_tls::TlsAcceptor::builder(identity)
            .min_protocol_version(Some(native_tls::Protocol::Tlsv12))
            .max_protocol_version(Some(native_tls::Protocol::Tlsv12))
            .build()
            .unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut stream = tokio_native_tls::TlsAcceptor::from(acceptor)
                .accept(socket)
                .await
                .unwrap();
            assert_eq!(stream.read_u8().await.unwrap(), 42);
        });
        let validation = TlsValidationConfig {
            accept_invalid_certs: false,
            accept_invalid_hostnames: false,
            use_alpn: false,
            server_ca_path: Some("tests/test_certificates/ca_cert.pem".into()),
        };
        let client = async {
            let socket = tokio::net::TcpStream::connect(addr).await.unwrap();
            let mut stream = crate::connection::transport::tls::default_engine(&validation)
                .connect(
                    Box::new(socket),
                    TlsConnectParams {
                        validation: &validation,
                        host_name: "localhost",
                        server_host_name: "localhost",
                        server_certificate_path: None,
                    },
                )
                .await
                .unwrap();
            let token = stream.channel_binding_token().expect("Schannel CBT");
            assert!(token.len() >= 32, "SEC_CHANNEL_BINDINGS header");
            let len = u32::from_le_bytes(token[24..28].try_into().unwrap()) as usize;
            let offset = u32::from_le_bytes(token[28..32].try_into().unwrap()) as usize;
            assert!(len > 0);
            assert!(offset >= 32 && offset + len <= token.len());
            stream.write_u8(42).await.unwrap();
            server.await.unwrap();
        };
        tokio::time::timeout(std::time::Duration::from_secs(30), client)
            .await
            .expect("custom CA handshake timed out");
    }

    #[test]
    fn cred_kind_for_trust_server_certificate_is_no_validate() {
        let validation = TlsValidationConfig {
            accept_invalid_certs: true,
            accept_invalid_hostnames: false,
            use_alpn: false,
            server_ca_path: None,
        };
        assert_eq!(pick_cred_kind(&validation, false), CredKind::NoValidate);
    }

    #[test]
    fn cred_kind_for_pinned_cert_is_manual_even_with_accept_invalid_certs() {
        // ServerCertificate=<path> triggers accept_invalid_certs=true in
        // ssl_handler::resolve_tls_validation, but for ODBC parity (and to
        // keep SSPI session-cache buckets separate) the pinned-cert path
        // must land in ManualValidate, not NoValidate.
        let validation = TlsValidationConfig {
            accept_invalid_certs: true,
            accept_invalid_hostnames: true,
            use_alpn: false,
            server_ca_path: None,
        };
        assert_eq!(pick_cred_kind(&validation, true), CredKind::ManualValidate);
    }

    #[test]
    fn cred_kind_for_pinned_cert_is_manual() {
        let validation = TlsValidationConfig {
            accept_invalid_certs: false,
            accept_invalid_hostnames: false,
            use_alpn: false,
            server_ca_path: None,
        };
        assert_eq!(pick_cred_kind(&validation, true), CredKind::ManualValidate);
    }

    #[test]
    fn cred_kind_default_is_auto() {
        let validation = TlsValidationConfig {
            accept_invalid_certs: false,
            accept_invalid_hostnames: false,
            use_alpn: false,
            server_ca_path: None,
        };
        assert_eq!(pick_cred_kind(&validation, false), CredKind::AutoValidate);
    }

    #[tokio::test]
    async fn connect_returns_handshake_error_when_peer_drops() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let mut tmp = vec![0u8; 8192];
            let _ = sock.readable().await;
            let _ = sock.try_read(&mut tmp);
            drop(sock);
        });

        let client = tokio::net::TcpStream::connect(addr).await.unwrap();
        // use_alpn=true exercises the ALPN-buffer branch; accept_invalid_certs
        // selects NoValidate so cred acquisition succeeds before the handshake
        // fails on the dropped socket.
        let validation = TlsValidationConfig {
            accept_invalid_certs: true,
            accept_invalid_hostnames: false,
            use_alpn: true,
            server_ca_path: None,
        };
        let params = TlsConnectParams {
            validation: &validation,
            host_name: "127.0.0.1",
            server_host_name: "127.0.0.1",
            server_certificate_path: None,
        };
        let result = SCHANNEL_ENGINE.connect(Box::new(client), params).await;
        assert!(result.is_err(), "expected handshake failure on peer drop");
        assert!(matches!(
            result,
            Err(crate::error::Error::ImplementationError(_))
        ));

        server.await.unwrap();
    }
}
