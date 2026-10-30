// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Strict (TDS 8.0) connections on macOS, made by Network.framework.
//!
//! SQL Server negotiates TLS 1.3 only on TDS 8.0 (Strict) connections, where the
//! TLS handshake starts as soon as TCP connects. `native-tls` on macOS wraps
//! Secure Transport, which stops at TLS 1.2 and has no post-quantum key exchange,
//! so those connections are made by Network.framework instead. Apple's TLS stack
//! negotiates TLS 1.3 and, on macOS 26 and later, the `X25519MLKEM768` hybrid
//! group, with no cryptography from outside the operating system. Certificate
//! chains are evaluated by Security.framework (see [`verify`]), so the system
//! trust store keeps deciding which servers are trusted.
//!
//! Network.framework cannot take over a socket someone else connected, so it
//! makes the TCP connection too, to one resolved address at a time, with the
//! options the socket transport sets on its own sockets. A [`Connector`] stands
//! in for the socket connect in the sequential and MultiSubnetFailover address
//! walks, and tells an address that could not be reached, which the walk moves
//! past, from a server that was reached but failed the handshake, which ends it.
//!
//! Handshakes tunneled through PreLogin packets (TDS 7.x) cannot use TLS 1.3, and
//! their transport is a framing layer rather than a connection Network.framework
//! could make, so they stay on native-tls.

mod ffi;
mod stream;
mod verify;

use std::error::Error as StdError;
use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tracing::{debug, error, info};

use self::stream::{KeepAlive, NwStream, Options};
use self::verify::Verifier;
use super::{TlsConnectParams, TlsValidationConfig};
use crate::connection::transport::certificate_validator;
use crate::connection::transport::network_transport::Stream;
use crate::connection::transport::parallel_connect::AttemptError;
use crate::core::TDS_8_ALPN_PROTOCOL;
use crate::error::Error;

type BoxedError = Box<dyn StdError + Send + Sync>;

/// Makes Strict connections to one server, one address at a time.
#[derive(Clone)]
pub(crate) struct Connector(Arc<Target>);

struct Target {
    validation: TlsValidationConfig,
    host_name: String,
    server_host_name: String,
    server_certificate_path: Option<PathBuf>,
    keep_alive: KeepAlive,
}

impl Connector {
    pub(crate) fn new(
        params: &TlsConnectParams<'_>,
        keep_alive_in_ms: u32,
        keep_alive_interval_in_ms: u32,
    ) -> Self {
        Self(Arc::new(Target {
            validation: params.validation.clone(),
            host_name: params.host_name.to_owned(),
            server_host_name: params.server_host_name.to_owned(),
            server_certificate_path: params.server_certificate_path.cloned(),
            keep_alive: KeepAlive {
                idle: Duration::from_millis(keep_alive_in_ms.into()),
                interval: Duration::from_millis(keep_alive_interval_in_ms.into()),
            },
        }))
    }

    /// Connects to `address` and completes the TLS handshake on it.
    pub(crate) fn connect(
        &self,
        address: SocketAddr,
    ) -> impl Future<Output = Result<Box<dyn Stream>, AttemptError>> + Send + 'static {
        let target = Arc::clone(&self.0);
        async move { target.connect(address).await }
    }
}

impl Target {
    async fn connect(&self, address: SocketAddr) -> Result<Box<dyn Stream>, AttemptError> {
        info!(
            "Starting TLS handshake to {} at {} using host {}",
            self.server_host_name, address, self.host_name
        );
        let verifier = Arc::new(Verifier::new(
            &self.validation,
            &self.host_name,
            self.server_certificate_path.is_some(),
        ));
        let options = Options {
            address,
            server_name: sni_host_name(&self.host_name),
            alpn: TDS_8_ALPN_PROTOCOL,
            keep_alive: self.keep_alive,
        };
        let stream =
            NwStream::start(&options, Arc::clone(&verifier)).map_err(|e| self.rejected(e))?;

        if let Err(failure) = stream.established().await {
            if let Some(reason) = verifier.take_failure() {
                return Err(self.rejected(reason));
            }
            if failure.is_unreachable() {
                debug!("Could not reach {}: {}", address, failure);
                return Err(AttemptError::Unreachable(failure.to_io_error()));
            }
            return Err(self.rejected(failure));
        }

        match stream.negotiated() {
            Some(negotiated) => info!("TLS handshake complete: {negotiated}"),
            None => info!("TLS handshake complete"),
        }

        if let Some(cert_path) = &self.server_certificate_path {
            info!("Validating server certificate using: {cert_path:?}");

            let leaf = verifier
                .take_leaf()
                .ok_or(AttemptError::Rejected(Error::NoServerCertificate))?;
            certificate_validator::validate_server_certificate(cert_path, &leaf)
                .map_err(AttemptError::Rejected)?;

            info!("Server certificate validation successful");
        }

        Ok(Box::new(stream))
    }

    fn rejected(&self, source: impl Into<BoxedError>) -> AttemptError {
        let source = source.into();
        error!(
            "TLS handshake FAILED: host_name={}, server_host_name={}, error={:?}",
            self.host_name, self.server_host_name, source,
        );
        AttemptError::Rejected(Error::TlsHandshakeError {
            source,
            expected_host: self.host_name.clone(),
        })
    }
}

/// The name to send as SNI, which RFC 6066 limits to DNS host names: no literal
/// addresses, and no trailing dot.
fn sni_host_name(host: &str) -> Option<&str> {
    let host = host.trim_end_matches('.');
    let usable = !host.is_empty()
        && host.is_ascii()
        && !host.contains(':')
        && host.parse::<Ipv4Addr>().is_err();
    usable.then_some(host)
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io;
    use std::path::Path;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::oneshot;
    use tokio::task::JoinHandle;
    use tokio_native_tls::{TlsAcceptor, TlsStream};

    use super::*;
    use crate::connection::transport::parallel_connect::parallel_connect_with;

    const HOST: &str = "localhost";
    const TIMEOUT: Duration = Duration::from_secs(30);
    const MEGABYTE: usize = 1024 * 1024;

    async fn within<T>(future: impl Future<Output = T>) -> T {
        tokio::time::timeout(TIMEOUT, future)
            .await
            .expect("timed out")
    }

    pub(crate) fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("test_certificates")
            .join(name)
    }

    /// A Secure Transport server presenting the self-signed test certificate.
    /// It speaks TLS 1.2 at most, which is all these tests need of a peer.
    fn acceptor() -> TlsAcceptor {
        let cert = std::fs::read(fixture("valid_cert.pem")).unwrap();
        let key = std::fs::read(fixture("key.pem")).unwrap();
        let identity = mssql_mock_tds::create_test_identity(&cert, &key).unwrap();
        TlsAcceptor::from(native_tls::TlsAcceptor::new(identity).unwrap())
    }

    pub(crate) async fn listen() -> (TcpListener, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        (listener, address)
    }

    /// Accepts one connection, running `session` on it as raw TCP.
    fn serve_tcp<F, Fut, T>(listener: TcpListener, session: F) -> JoinHandle<T>
    where
        F: FnOnce(TcpStream) -> Fut + Send + 'static,
        Fut: Future<Output = T> + Send,
        T: Send + 'static,
    {
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            session(tcp).await
        })
    }

    /// Accepts one connection, running `session` on it once TLS is up.
    pub(crate) fn serve<F, Fut, T>(listener: TcpListener, session: F) -> JoinHandle<T>
    where
        F: FnOnce(TlsStream<TcpStream>) -> Fut + Send + 'static,
        Fut: Future<Output = T> + Send,
        T: Send + 'static,
    {
        serve_tcp(listener, |tcp| async move {
            let tls = acceptor().accept(tcp).await.expect("server handshake");
            session(tls).await
        })
    }

    /// Holds a server's connection until the client ends it.
    ///
    /// Network.framework can fail a handshake whose server hangs up right behind
    /// its last flight, so a server must not close before the client is connected.
    pub(crate) async fn until_the_client_leaves(mut tls: TlsStream<TcpStream>) {
        let _ = tls.read(&mut [0]).await;
    }

    fn trusting() -> TlsValidationConfig {
        TlsValidationConfig {
            accept_invalid_certs: true,
            accept_invalid_hostnames: true,
            use_alpn: true,
        }
    }

    fn validating() -> TlsValidationConfig {
        TlsValidationConfig {
            accept_invalid_certs: false,
            accept_invalid_hostnames: false,
            use_alpn: true,
        }
    }

    fn connector(validation: &TlsValidationConfig, pin: Option<&PathBuf>) -> Connector {
        let params = TlsConnectParams {
            validation,
            host_name: HOST,
            server_host_name: HOST,
            server_certificate_path: pin,
        };
        Connector::new(&params, 30_000, 1_000)
    }

    async fn connect(
        address: SocketAddr,
        validation: &TlsValidationConfig,
        pin: Option<&PathBuf>,
    ) -> Result<Box<dyn Stream>, AttemptError> {
        within(connector(validation, pin).connect(address)).await
    }

    async fn rejection(
        address: SocketAddr,
        validation: &TlsValidationConfig,
        pin: Option<&PathBuf>,
    ) -> Error {
        match connect(address, validation, pin).await {
            Err(AttemptError::Rejected(error)) => error,
            Err(error) => panic!("expected a rejection, got {error:?}"),
            Ok(_) => panic!("the connection should have failed"),
        }
    }

    /// An address nothing listens on.
    pub(crate) async fn refusing_address() -> SocketAddr {
        let (listener, address) = listen().await;
        drop(listener);
        address
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[tokio::test]
    async fn echoes_over_a_direct_connection() {
        let (listener, address) = listen().await;
        let server = serve(listener, |mut tls| async move {
            let mut request = [0; 5];
            tls.read_exact(&mut request).await.unwrap();
            tls.write_all(&request).await.unwrap();
            tls.flush().await.unwrap();
            let mut trailing = Vec::new();
            tls.read_to_end(&mut trailing).await.unwrap();
            trailing
        });

        let mut stream = connect(address, &trusting(), None).await.unwrap();
        stream.write_all(b"hello").await.unwrap();
        stream.flush().await.unwrap();
        let mut reply = [0; 5];
        within(stream.read_exact(&mut reply)).await.unwrap();
        assert_eq!(&reply, b"hello");
        assert!(!stream.is_connection_dead());

        within(stream.shutdown()).await.unwrap();
        assert!(within(server).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn transfers_large_payloads_in_both_directions() {
        let (listener, address) = listen().await;
        let server = serve(listener, |tls| async move {
            let (mut read, mut write) = tokio::io::split(tls);
            let upload = async {
                let mut received = vec![0; MEGABYTE];
                read.read_exact(&mut received).await.unwrap();
                received
            };
            let download = async {
                write.write_all(&pattern(MEGABYTE)).await.unwrap();
                write.flush().await.unwrap();
            };
            let (received, ()) = tokio::join!(upload, download);
            let mut trailing = Vec::new();
            read.read_to_end(&mut trailing).await.unwrap();
            received
        });

        let stream = connect(address, &trusting(), None).await.unwrap();
        let (mut read, mut write) = tokio::io::split(stream);
        let upload = async {
            write.write_all(&pattern(MEGABYTE)).await.unwrap();
            write.flush().await.unwrap();
        };
        let download = async {
            let mut received = vec![0; MEGABYTE];
            read.read_exact(&mut received).await.unwrap();
            received
        };
        let (received, ()) = within(async { tokio::join!(download, upload) }).await;
        assert!(received == pattern(MEGABYTE));

        within(write.shutdown()).await.unwrap();
        assert!(within(server).await.unwrap() == pattern(MEGABYTE));
    }

    #[tokio::test]
    async fn shutdown_delivers_everything_written_before_it() {
        let (listener, address) = listen().await;
        let server = serve(listener, |mut tls| async move {
            let mut received = Vec::new();
            tls.read_to_end(&mut received).await.unwrap();
            received
        });

        let mut stream = connect(address, &trusting(), None).await.unwrap();
        stream.write_all(&pattern(MEGABYTE)).await.unwrap();
        within(stream.shutdown()).await.unwrap();

        let received = within(server).await.unwrap();
        assert_eq!(received.len(), MEGABYTE);
        assert!(received == pattern(MEGABYTE));
    }

    #[tokio::test]
    async fn dropping_the_stream_closes_the_connection() {
        let (listener, address) = listen().await;
        let server = serve(listener, |mut tls| async move {
            let mut received = Vec::new();
            tls.read_to_end(&mut received).await.unwrap();
            received
        });

        let mut stream = connect(address, &trusting(), None).await.unwrap();
        stream.write_all(b"goodbye").await.unwrap();
        drop(stream);

        assert_eq!(within(server).await.unwrap(), b"goodbye");
    }

    /// A stream whose server stopped reading after the handshake, written to until
    /// it takes no more, along with a way to tell when its connection has ended.
    async fn stalled_stream() -> (
        NwStream,
        impl Fn() -> bool,
        oneshot::Sender<()>,
        JoinHandle<()>,
    ) {
        let (release, released) = oneshot::channel::<()>();
        let (listener, address) = listen().await;
        let server = serve(listener, |tls| async move {
            let _ = released.await;
            drop(tls);
        });
        let options = Options {
            address,
            server_name: Some(HOST),
            alpn: TDS_8_ALPN_PROTOCOL,
            keep_alive: KeepAlive {
                idle: Duration::from_secs(30),
                interval: Duration::from_secs(1),
            },
        };
        let verifier = Arc::new(Verifier::new(&trusting(), HOST, false));
        let mut stream = NwStream::start(&options, verifier).unwrap();
        within(stream.established()).await.unwrap();
        let chunk = vec![0; 64 * 1024];
        within(async {
            while let Ok(written) =
                tokio::time::timeout(Duration::from_millis(250), stream.write(&chunk)).await
            {
                written.unwrap();
            }
        })
        .await;
        let ended = stream.end_probe();
        (stream, ended, release, server)
    }

    async fn ends_soon(ended: impl Fn() -> bool) {
        let wait = async {
            while !ended() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        tokio::time::timeout(Duration::from_secs(5), wait)
            .await
            .expect("the connection was never cancelled");
    }

    #[tokio::test]
    async fn dropping_a_stream_the_server_stopped_reading_still_ends_it() {
        let (stream, ended, release, server) = stalled_stream().await;
        drop(stream);

        ends_soon(ended).await;
        release.send(()).unwrap();
        within(server).await.unwrap();
    }

    #[tokio::test]
    async fn dropping_a_stream_after_a_stalled_shutdown_still_ends_it() {
        let (mut stream, ended, release, server) = stalled_stream().await;
        let shutdown = tokio::time::timeout(Duration::from_millis(250), stream.shutdown()).await;
        assert!(shutdown.is_err(), "the shutdown should wait for the server");
        drop(stream);

        ends_soon(ended).await;
        release.send(()).unwrap();
        within(server).await.unwrap();
    }

    #[tokio::test]
    async fn operations_after_shutdown_report_the_closed_connection() {
        let (listener, address) = listen().await;
        let server = serve(listener, |mut tls| async move {
            let mut received = Vec::new();
            tls.read_to_end(&mut received).await.unwrap();
        });

        let mut stream = connect(address, &trusting(), None).await.unwrap();
        within(stream.shutdown()).await.unwrap();
        within(stream.shutdown()).await.unwrap();

        assert!(stream.is_connection_dead());
        assert!(stream.write_all(b"late").await.is_err());
        let mut received = Vec::new();
        assert_eq!(within(stream.read_to_end(&mut received)).await.unwrap(), 0);
        within(server).await.unwrap();
    }

    #[tokio::test]
    async fn a_clean_server_close_is_the_end_of_the_stream() {
        let (listener, address) = listen().await;
        let (connected, client_connected) = oneshot::channel::<()>();
        let server = serve(listener, |mut tls| async move {
            client_connected.await.unwrap();
            tls.write_all(b"bye").await.unwrap();
            tls.shutdown().await.unwrap();
        });

        let mut stream = connect(address, &trusting(), None).await.unwrap();
        connected.send(()).unwrap();
        let mut received = Vec::new();
        within(stream.read_to_end(&mut received)).await.unwrap();

        assert_eq!(received, b"bye");
        assert!(stream.is_connection_dead());
        within(server).await.unwrap();
    }

    // Network.framework reports a connection that ends without `close_notify` as a
    // clean end of stream, as `security-framework` does for the native-tls engine.
    // It is the TDS framing that notices a message cut short.
    #[tokio::test]
    async fn a_close_without_close_notify_ends_the_stream() {
        let (listener, address) = listen().await;
        let (connected, client_connected) = oneshot::channel::<()>();
        let server = serve(listener, |mut tls| async move {
            client_connected.await.unwrap();
            tls.write_all(b"partial").await.unwrap();
            tls.flush().await.unwrap();
            // Dropped without `shutdown`, so the peer sees TCP end instead.
        });

        let mut stream = connect(address, &trusting(), None).await.unwrap();
        connected.send(()).unwrap();
        within(server).await.unwrap();
        let mut received = Vec::new();
        within(stream.read_to_end(&mut received)).await.unwrap();

        assert_eq!(received, b"partial");
        assert!(stream.is_connection_dead());
    }

    #[tokio::test]
    async fn an_idle_connection_notices_the_server_hanging_up() {
        let (listener, address) = listen().await;
        let (hang_up, hung_up) = oneshot::channel::<()>();
        let server = serve(listener, |mut tls| async move {
            tls.write_all(b"ready").await.unwrap();
            tls.flush().await.unwrap();
            hung_up.await.unwrap();
            tls.shutdown().await.unwrap();
        });

        let mut stream = connect(address, &trusting(), None).await.unwrap();
        let mut ready = [0; 5];
        within(stream.read_exact(&mut ready)).await.unwrap();
        assert!(!stream.is_connection_dead());

        hang_up.send(()).unwrap();
        within(server).await.unwrap();
        within(async {
            while !stream.is_connection_dead() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
    }

    #[tokio::test]
    async fn a_refused_connection_is_unreachable_as_a_socket_reports_it() {
        let address = refusing_address().await;

        let error = match connect(address, &trusting(), None).await {
            Err(AttemptError::Unreachable(error)) => error,
            Err(error) => panic!("expected an unreachable address, got {error:?}"),
            Ok(_) => panic!("nothing listens there"),
        };

        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
        assert_eq!(error.raw_os_error(), Some(libc::ECONNREFUSED));
        assert!(Error::from(error).is_transient_connect_error());
    }

    #[tokio::test]
    async fn the_race_moves_past_an_address_that_refuses() {
        let refused = refusing_address().await;
        let (listener, live) = listen().await;
        let server = serve(listener, |mut tls| async move {
            let mut received = Vec::new();
            tls.read_to_end(&mut received).await.unwrap();
        });
        let connector = connector(&trusting(), None);

        let result = within(parallel_connect_with(
            vec![refused, live],
            TIMEOUT.as_millis() as u64,
            |address| connector.connect(address),
        ))
        .await
        .unwrap();
        assert_eq!(result.connected_address, live);
        assert_eq!(result.total_addresses, 2);

        let mut stream = result.stream;
        within(stream.shutdown()).await.unwrap();
        within(server).await.unwrap();
    }

    #[tokio::test]
    async fn rejects_an_untrusted_certificate_when_validating() {
        let (listener, address) = listen().await;
        let server = serve_tcp(listener, |tcp| async move {
            acceptor().accept(tcp).await.is_err()
        });

        let error = rejection(address, &validating(), None).await;

        let Error::TlsHandshakeError {
            source,
            expected_host,
        } = error
        else {
            panic!("expected a TLS handshake error, got {error:?}");
        };
        assert_eq!(expected_host, HOST);
        assert!(!source.to_string().is_empty());
        assert!(within(server).await.unwrap(), "the server saw a handshake");
    }

    #[tokio::test]
    async fn accepts_a_certificate_matching_the_pin() {
        for name in ["valid_cert.der", "valid_cert.pem"] {
            let (listener, address) = listen().await;
            let server = serve(listener, |mut tls| async move {
                let mut received = Vec::new();
                tls.read_to_end(&mut received).await.unwrap();
            });

            let pin = fixture(name);
            let mut stream = connect(address, &trusting(), Some(&pin)).await.unwrap();
            within(stream.shutdown()).await.unwrap();
            within(server).await.unwrap();
        }
    }

    #[tokio::test]
    async fn rejects_a_certificate_that_is_not_the_pin() {
        let mut tampered = std::fs::read(fixture("valid_cert.der")).unwrap();
        // The last byte belongs to the signature, so the file still parses.
        *tampered.last_mut().unwrap() ^= 0xff;
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), tampered).unwrap();

        let (listener, address) = listen().await;
        let server = serve(listener, until_the_client_leaves);

        let error = rejection(address, &trusting(), Some(&file.path().to_path_buf())).await;

        assert!(matches!(error, Error::CertificateMismatch), "{error:?}");
        within(server).await.unwrap();
    }

    #[tokio::test]
    async fn reports_a_missing_pin_file() {
        let (listener, address) = listen().await;
        let server = serve(listener, until_the_client_leaves);

        let error = rejection(
            address,
            &trusting(),
            Some(&fixture("no_such_certificate.pem")),
        )
        .await;

        assert!(
            matches!(error, Error::CertificateNotFound { .. }),
            "{error:?}"
        );
        within(server).await.unwrap();
    }

    #[tokio::test]
    async fn fails_when_the_peer_does_not_speak_tls() {
        let (listener, address) = listen().await;
        let server = serve_tcp(listener, |mut tcp| async move {
            let mut hello = [0; 5];
            tcp.read_exact(&mut hello).await.unwrap();
            tcp.write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n")
                .await
                .unwrap();
            // Held open, so the failure has to come from what was received.
            let mut rest = Vec::new();
            let _ = tcp.read_to_end(&mut rest).await;
        });

        let error = rejection(address, &trusting(), None).await;

        assert!(
            matches!(error, Error::TlsHandshakeError { .. }),
            "{error:?}"
        );
        within(server).await.unwrap();
    }

    #[tokio::test]
    async fn fails_when_the_peer_hangs_up_during_the_handshake() {
        let (listener, address) = listen().await;
        let server = serve_tcp(listener, |mut tcp| async move {
            let mut hello = [0; 5];
            tcp.read_exact(&mut hello).await.unwrap();
        });

        let error = rejection(address, &trusting(), None).await;

        assert!(
            matches!(error, Error::TlsHandshakeError { .. }),
            "{error:?}"
        );
        within(server).await.unwrap();
    }

    #[test]
    fn sni_names_are_dns_host_names_only() {
        for (host, expected) in [
            ("sql.example.com", Some("sql.example.com")),
            ("sql.example.com.", Some("sql.example.com")),
            ("localhost", Some("localhost")),
            ("127.0.0.1", None),
            ("::1", None),
            ("[::1]", None),
            ("fe80::1%en0", None),
            ("bücher.example", None),
            ("", None),
            (".", None),
        ] {
            assert_eq!(sni_host_name(host), expected, "{host:?}");
        }
    }
}
