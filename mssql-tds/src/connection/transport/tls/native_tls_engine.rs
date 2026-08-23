// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The `native-tls`-backed [`TlsEngine`] implementation.
//!
//! This is the implementation that has shipped with `mssql-tds` since
//! day one. The code in this file was extracted verbatim from
//! `ssl_handler::SslHandler::enable_ssl_async` as part of the TLS-engine
//! refactor; no behavior changed.

use std::collections::HashMap;
use std::sync::RwLock;

use async_trait::async_trait;
use native_tls::TlsConnector as NativeTlsConnector;
use tokio_native_tls::TlsStream;
use tracing::{debug, error, info};

use super::{TlsConnectParams, TlsEngine};
use crate::connection::transport::certificate_validator;
use crate::connection::transport::network_transport::Stream;
use crate::core::{TDS_8_ALPN_PROTOCOL, TdsResult};

/// Cache of pre-built `NativeTlsConnector` instances keyed by validation config.
/// Building a connector is expensive (~50ms on Linux) because `native-tls` loads
/// and parses the system CA certificate store via OpenSSL on every call to
/// `builder().build()`. Caching avoids this cost on subsequent connections.
static CONNECTOR_CACHE: std::sync::LazyLock<
    RwLock<HashMap<super::TlsValidationConfig, NativeTlsConnector>>,
> = std::sync::LazyLock::new(|| RwLock::new(HashMap::new()));

fn get_or_build_connector(
    validation: &super::TlsValidationConfig,
) -> TdsResult<NativeTlsConnector> {
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

/// Zero-sized engine type. There is exactly one instance, the static
/// [`NATIVE_TLS_ENGINE`], referenced from [`super::default_engine`].
#[derive(Debug)]
pub(crate) struct NativeTlsEngine;

pub(crate) static NATIVE_TLS_ENGINE: NativeTlsEngine = NativeTlsEngine;

#[async_trait]
impl TlsEngine for NativeTlsEngine {
    async fn connect(
        &self,
        mut base_stream: Box<dyn Stream>,
        params: TlsConnectParams<'_>,
    ) -> TdsResult<Box<dyn Stream>> {
        base_stream.tls_handshake_starting();

        let connector = get_or_build_connector(params.validation)?;

        info!(
            "Starting TLS handshake to {} using host {}",
            params.server_host_name, params.host_name
        );
        let encrypted_stream = tokio_native_tls::TlsConnector::from(connector)
            .connect(params.host_name, base_stream)
            .await;

        match encrypted_stream {
            Ok(mut stream) => {
                if params.validation.use_alpn {
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

                if let Some(cert_path) = params.server_certificate_path {
                    info!("Validating server certificate using: {cert_path:?}",);

                    let peer_cert = stream
                        .get_ref()
                        .peer_certificate()
                        .map_err(crate::error::Error::TlsError)?
                        .ok_or(crate::error::Error::NoServerCertificate)?;

                    let server_cert_der =
                        peer_cert.to_der().map_err(crate::error::Error::TlsError)?;

                    certificate_validator::validate_server_certificate(
                        cert_path,
                        &server_cert_der,
                    )?;

                    info!("Server certificate validation successful");
                }

                stream
                    .get_mut()
                    .get_mut()
                    .get_mut()
                    .tls_handshake_completed();
                Ok(Box::new(stream))
            }
            Err(e) => {
                error!(
                    "TLS handshake FAILED: host_name={}, server_host_name={}, error={:?}, os={}",
                    params.host_name,
                    params.server_host_name,
                    e,
                    std::env::consts::OS,
                );
                Err(crate::error::Error::TlsHandshakeError {
                    source: e,
                    expected_host: params.host_name.to_string(),
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
        // get_mut() three times reaches the underlying Box<dyn Stream>.
        self.get_mut().get_mut().get_mut().tls_handshake_starting();
    }

    fn tls_handshake_completed(&mut self) {
        self.get_mut().get_mut().get_mut().tls_handshake_completed();
    }

    fn is_connection_dead(&self) -> bool {
        self.get_ref().get_ref().get_ref().is_connection_dead()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::transport::tls::TlsValidationConfig;

    fn cfg(certs: bool, hosts: bool, alpn: bool) -> TlsValidationConfig {
        TlsValidationConfig {
            accept_invalid_certs: certs,
            accept_invalid_hostnames: hosts,
            use_alpn: alpn,
        }
    }

    #[test]
    fn builds_connector_for_strict_config() {
        assert!(get_or_build_connector(&cfg(false, false, false)).is_ok());
    }

    #[test]
    fn builds_connector_with_all_danger_flags_and_alpn() {
        assert!(get_or_build_connector(&cfg(true, true, true)).is_ok());
    }

    #[test]
    fn second_call_with_same_config_hits_cache() {
        let c = cfg(true, false, true);
        // First call builds and inserts; second call must take the cache-hit
        // early-return branch.
        assert!(get_or_build_connector(&c).is_ok());
        assert!(get_or_build_connector(&c).is_ok());
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
        let validation = cfg(false, false, true);
        let params = TlsConnectParams {
            validation: &validation,
            host_name: "127.0.0.1",
            server_host_name: "127.0.0.1",
            server_certificate_path: None,
        };
        let result = NATIVE_TLS_ENGINE.connect(Box::new(client), params).await;
        assert!(result.is_err(), "expected handshake failure on peer drop");

        server.await.unwrap();
    }
}
