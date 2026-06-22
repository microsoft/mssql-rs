// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The Schannel-direct [`TlsEngine`] implementation.
//!
//! Stitches together [`super::cred`], [`super::handshake`],
//! [`super::stream::SchannelTlsStream`], and [`super::validate`] into a
//! drop-in replacement for [`super::super::tls::native_tls_engine`].
//!
//! Enabled by default on Windows via the `tls-schannel-direct` Cargo
//! feature; [`super::super::tls::default_engine`] routes to it.

use async_trait::async_trait;
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
    if has_pinned_cert {
        // ServerCertificate=<path>: file-pin DER compare runs post-
        // handshake. Schannel chain build is skipped via ISC bit.
        // Separate cache bucket from NoValidate (ODBC parity).
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
        let cred = cred::get_or_acquire(kind).map_err(|e| {
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

        if let Err(e) = validate::validate_after_handshake(
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cred_kind_for_trust_server_certificate_is_no_validate() {
        let validation = TlsValidationConfig {
            accept_invalid_certs: true,
            accept_invalid_hostnames: false,
            use_alpn: false,
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
        };
        assert_eq!(pick_cred_kind(&validation, true), CredKind::ManualValidate);
    }

    #[test]
    fn cred_kind_for_pinned_cert_is_manual() {
        let validation = TlsValidationConfig {
            accept_invalid_certs: false,
            accept_invalid_hostnames: false,
            use_alpn: false,
        };
        assert_eq!(pick_cred_kind(&validation, true), CredKind::ManualValidate);
    }

    #[test]
    fn cred_kind_default_is_auto() {
        let validation = TlsValidationConfig {
            accept_invalid_certs: false,
            accept_invalid_hostnames: false,
            use_alpn: false,
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
