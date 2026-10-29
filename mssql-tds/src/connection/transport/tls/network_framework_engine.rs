// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The macOS [`TlsEngine`]: Network.framework for direct TDS 8.0 handshakes,
//! native-tls for the rest.
//!
//! SQL Server negotiates TLS 1.3 only on TDS 8.0 (Strict) connections, where the
//! handshake runs straight on the socket. `native-tls` on macOS wraps Secure
//! Transport, which stops at TLS 1.2 and has no post-quantum key exchange, so
//! those handshakes run in Network.framework instead. Apple's TLS stack
//! negotiates TLS 1.3 and, on macOS 26 and later, the `X25519MLKEM768` hybrid
//! group, with no cryptography from outside the operating system. Certificate
//! chains are evaluated by Security.framework (see [`verify`]), so the system
//! trust store keeps deciding which servers are trusted.
//!
//! Network.framework cannot be handed a socket or any other byte stream, only an
//! endpoint to connect to. The engine therefore points it at a private Unix
//! domain socket and relays the TLS records between that socket and the transport
//! the connection layer established (see [`bridge`]). The cost is one extra hop
//! through the kernel in each direction.
//!
//! Handshakes tunneled through PreLogin packets (TDS 7.x) cannot use TLS 1.3, and
//! their transport is a framing layer rather than a socket Network.framework
//! could reach, so they stay on native-tls.

mod bridge;
mod ffi;
mod stream;
mod verify;

use std::error::Error as StdError;
use std::net::Ipv4Addr;
use std::sync::Arc;

use async_trait::async_trait;
use tracing::{error, info};

use self::bridge::{Bridge, Health, spawn_pump};
use self::stream::{Failure, NwStream, Options};
use self::verify::Verifier;
use super::native_tls_engine::NATIVE_TLS_ENGINE;
use super::{TlsConnectParams, TlsEngine};
use crate::connection::transport::certificate_validator;
use crate::connection::transport::network_transport::Stream;
use crate::core::{TDS_8_ALPN_PROTOCOL, TdsResult};
use crate::error::Error;

/// Zero-sized engine type. There is exactly one instance, the static
/// [`NETWORK_FRAMEWORK_ENGINE`], referenced from [`super::default_engine`].
#[derive(Debug)]
pub(crate) struct NetworkFrameworkEngine;

pub(crate) static NETWORK_FRAMEWORK_ENGINE: NetworkFrameworkEngine = NetworkFrameworkEngine;

type BoxedError = Box<dyn StdError + Send + Sync>;

/// Turns a handshake failure into the error surfaced to callers.
fn handshake_error(source: impl Into<BoxedError>, host: &str) -> Error {
    Error::TlsHandshakeError {
        source: source.into(),
        expected_host: host.to_string(),
    }
}

#[async_trait]
impl TlsEngine for NetworkFrameworkEngine {
    async fn connect(
        &self,
        mut base_stream: Box<dyn Stream>,
        params: TlsConnectParams<'_>,
    ) -> TdsResult<Box<dyn Stream>> {
        // `use_alpn` is set exactly for the direct TDS 8.0 (Strict) handshake.
        if !params.validation.use_alpn {
            return NATIVE_TLS_ENGINE.connect(base_stream, params).await;
        }

        base_stream.tls_handshake_starting();

        info!(
            "Starting TLS handshake to {} using host {}",
            params.server_host_name, params.host_name
        );
        let verifier = Arc::new(Verifier::new(
            params.validation,
            params.host_name,
            params.server_certificate_path.is_some(),
        ));
        let stream = handshake(base_stream, &params, &verifier)
            .await
            .map_err(|e| {
                error!(
                    "TLS handshake FAILED: host_name={}, server_host_name={}, error={:?}",
                    params.host_name, params.server_host_name, e,
                );
                handshake_error(e, params.host_name)
            })?;

        match stream.negotiated() {
            Some(negotiated) => info!("TLS handshake complete: {negotiated}"),
            None => info!("TLS handshake complete"),
        }

        if let Some(cert_path) = params.server_certificate_path {
            info!("Validating server certificate using: {cert_path:?}");

            let leaf = verifier.take_leaf().ok_or(Error::NoServerCertificate)?;
            certificate_validator::validate_server_certificate(cert_path, &leaf)?;

            info!("Server certificate validation successful");
        }

        // The relay owns the transport now, and no transport's completion hook does anything.
        Ok(Box::new(stream))
    }
}

async fn handshake(
    base_stream: Box<dyn Stream>,
    params: &TlsConnectParams<'_>,
    verifier: &Arc<Verifier>,
) -> Result<NwStream, BoxedError> {
    let bridge = Bridge::bind()?;
    let health = Arc::new(Health::default());
    let socket_path = bridge.socket_path();
    let mut stream = NwStream::start(
        &Options {
            socket_path: &socket_path,
            server_name: sni_host_name(params.host_name),
            alpn: TDS_8_ALPN_PROTOCOL,
        },
        Arc::clone(verifier),
        Arc::clone(&health),
    )?;

    // Network.framework connects before anything has been sent, so only a failure
    // to do so can end the wait for it.
    let socket = tokio::select! {
        accepted = bridge.accept() => accepted?,
        failure = stream.failed() => return Err(cause(failure, verifier, &health)),
    };
    stream.attach_pump(spawn_pump(socket, base_stream, Arc::clone(&health)));

    stream
        .established()
        .await
        .map_err(|failure| cause(failure, verifier, &health))?;
    Ok(stream)
}

/// The most specific reason a handshake failed: a rejected certificate, then the
/// transport failing, then whatever Network.framework reported.
fn cause(failure: Failure, verifier: &Verifier, health: &Health) -> BoxedError {
    if let Some(reason) = verifier.take_failure() {
        return reason.into();
    }
    if let Some(error) = health.error() {
        return error.into();
    }
    failure.into()
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
mod tests {
    use std::future::Future;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll};
    use std::time::Duration;

    use tokio::io::{
        AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, DuplexStream, ReadBuf, duplex,
    };
    use tokio::task::JoinHandle;
    use tokio_native_tls::{TlsAcceptor, TlsStream};

    use super::*;
    use crate::connection::transport::tls::TlsValidationConfig;

    const HOST: &str = "localhost";
    const TIMEOUT: Duration = Duration::from_secs(30);
    const PIPE_CAPACITY: usize = 64 * 1024;
    const MEGABYTE: usize = 1024 * 1024;

    async fn within<T>(future: impl Future<Output = T>) -> T {
        tokio::time::timeout(TIMEOUT, future)
            .await
            .expect("timed out")
    }

    fn fixture(name: &str) -> PathBuf {
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

    fn serve<F, Fut, T>(io: DuplexStream, session: F) -> JoinHandle<T>
    where
        F: FnOnce(TlsStream<DuplexStream>) -> Fut + Send + 'static,
        Fut: Future<Output = T> + Send,
        T: Send + 'static,
    {
        tokio::spawn(async move {
            let tls = acceptor().accept(io).await.expect("server handshake");
            session(tls).await
        })
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

    async fn connect(
        transport: impl Stream + 'static,
        validation: &TlsValidationConfig,
        pin: Option<&PathBuf>,
    ) -> TdsResult<Box<dyn Stream>> {
        let params = TlsConnectParams {
            validation,
            host_name: HOST,
            server_host_name: HOST,
            server_certificate_path: pin,
        };
        within(NETWORK_FRAMEWORK_ENGINE.connect(Box::new(transport), params)).await
    }

    async fn connect_failure(
        transport: impl Stream + 'static,
        validation: &TlsValidationConfig,
        pin: Option<&PathBuf>,
    ) -> Error {
        match connect(transport, validation, pin).await {
            Ok(_) => panic!("the handshake should have failed"),
            Err(error) => error,
        }
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    /// Counts how often the transport is told a handshake is starting.
    struct Probe {
        inner: DuplexStream,
        announced: Arc<AtomicUsize>,
    }

    impl AsyncRead for Probe {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for Probe {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Pin::new(&mut self.inner).poll_write(cx, buf)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_flush(cx)
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

    impl Stream for Probe {
        fn tls_handshake_starting(&mut self) {
            self.announced.fetch_add(1, Ordering::SeqCst);
        }

        fn tls_handshake_completed(&mut self) {}
    }

    /// A transport that has already failed.
    struct Broken;

    impl AsyncRead for Broken {
        fn poll_read(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            _buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Err(io::ErrorKind::ConnectionReset.into()))
        }
    }

    impl AsyncWrite for Broken {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl Stream for Broken {
        fn tls_handshake_starting(&mut self) {}

        fn tls_handshake_completed(&mut self) {}
    }

    #[tokio::test]
    async fn echoes_after_a_direct_handshake() {
        let (client_io, server_io) = duplex(PIPE_CAPACITY);
        let server = serve(server_io, |mut tls| async move {
            let mut request = [0; 5];
            tls.read_exact(&mut request).await.unwrap();
            tls.write_all(&request).await.unwrap();
            tls.flush().await.unwrap();
            let mut trailing = Vec::new();
            tls.read_to_end(&mut trailing).await.unwrap();
            trailing
        });

        let mut stream = connect(client_io, &trusting(), None).await.unwrap();
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
        let (client_io, server_io) = duplex(PIPE_CAPACITY);
        let server = serve(server_io, |tls| async move {
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

        let stream = connect(client_io, &trusting(), None).await.unwrap();
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
        let (client_io, server_io) = duplex(PIPE_CAPACITY);
        let server = serve(server_io, |mut tls| async move {
            let mut received = Vec::new();
            tls.read_to_end(&mut received).await.unwrap();
            received
        });

        let mut stream = connect(client_io, &trusting(), None).await.unwrap();
        stream.write_all(&pattern(MEGABYTE)).await.unwrap();
        within(stream.shutdown()).await.unwrap();

        let received = within(server).await.unwrap();
        assert_eq!(received.len(), MEGABYTE);
        assert!(received == pattern(MEGABYTE));
    }

    #[tokio::test]
    async fn dropping_the_stream_closes_the_connection() {
        let (client_io, server_io) = duplex(PIPE_CAPACITY);
        let server = serve(server_io, |mut tls| async move {
            let mut received = Vec::new();
            tls.read_to_end(&mut received).await.unwrap();
            received
        });

        let mut stream = connect(client_io, &trusting(), None).await.unwrap();
        stream.write_all(b"goodbye").await.unwrap();
        drop(stream);

        assert_eq!(within(server).await.unwrap(), b"goodbye");
    }

    #[tokio::test]
    async fn operations_after_shutdown_report_the_closed_connection() {
        let (client_io, server_io) = duplex(PIPE_CAPACITY);
        let server = serve(server_io, |mut tls| async move {
            let mut received = Vec::new();
            tls.read_to_end(&mut received).await.unwrap();
        });

        let mut stream = connect(client_io, &trusting(), None).await.unwrap();
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
        let (client_io, server_io) = duplex(PIPE_CAPACITY);
        let server = serve(server_io, |mut tls| async move {
            tls.write_all(b"bye").await.unwrap();
            tls.shutdown().await.unwrap();
        });

        let mut stream = connect(client_io, &trusting(), None).await.unwrap();
        let mut received = Vec::new();
        within(stream.read_to_end(&mut received)).await.unwrap();

        assert_eq!(received, b"bye");
        assert!(stream.is_connection_dead());
        within(server).await.unwrap();
    }

    // Network.framework reports a transport that ends without `close_notify` as a
    // clean end of stream, as `security-framework` does for the native-tls engine.
    // It is the TDS framing that notices a message cut short.
    #[tokio::test]
    async fn a_close_without_close_notify_ends_the_stream() {
        let (client_io, server_io) = duplex(PIPE_CAPACITY);
        let server = serve(server_io, |mut tls| async move {
            tls.write_all(b"partial").await.unwrap();
            tls.flush().await.unwrap();
            // Dropped without `shutdown`, so the peer sees the transport end instead.
        });

        let mut stream = connect(client_io, &trusting(), None).await.unwrap();
        within(server).await.unwrap();
        let mut received = Vec::new();
        within(stream.read_to_end(&mut received)).await.unwrap();

        assert_eq!(received, b"partial");
        assert!(stream.is_connection_dead());
    }

    #[tokio::test]
    async fn rejects_an_untrusted_certificate_when_validating() {
        let (client_io, server_io) = duplex(PIPE_CAPACITY);
        let server = tokio::spawn(async move { acceptor().accept(server_io).await.is_err() });

        let error = connect_failure(client_io, &validating(), None).await;

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
            let (client_io, server_io) = duplex(PIPE_CAPACITY);
            let server = serve(server_io, |mut tls| async move {
                let mut received = Vec::new();
                tls.read_to_end(&mut received).await.unwrap();
            });

            let pin = fixture(name);
            let mut stream = connect(client_io, &trusting(), Some(&pin)).await.unwrap();
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

        let (client_io, server_io) = duplex(PIPE_CAPACITY);
        let server = tokio::spawn(async move {
            let _ = acceptor().accept(server_io).await;
        });

        let error = connect_failure(client_io, &trusting(), Some(&file.path().to_path_buf())).await;

        assert!(matches!(error, Error::CertificateMismatch), "{error:?}");
        within(server).await.unwrap();
    }

    #[tokio::test]
    async fn reports_a_missing_pin_file() {
        let (client_io, server_io) = duplex(PIPE_CAPACITY);
        let server = tokio::spawn(async move {
            let _ = acceptor().accept(server_io).await;
        });

        let error = connect_failure(
            client_io,
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
        let (client_io, mut server_io) = duplex(PIPE_CAPACITY);
        let server = tokio::spawn(async move {
            let mut hello = [0; 5];
            server_io.read_exact(&mut hello).await.unwrap();
            server_io
                .write_all(b"HTTP/1.1 400 Bad Request\r\n\r\n")
                .await
                .unwrap();
            // Held open, so the failure has to come from what was received.
            let mut rest = Vec::new();
            let _ = server_io.read_to_end(&mut rest).await;
        });

        let error = connect_failure(client_io, &trusting(), None).await;

        assert!(
            matches!(error, Error::TlsHandshakeError { .. }),
            "{error:?}"
        );
        within(server).await.unwrap();
    }

    #[tokio::test]
    async fn fails_when_the_peer_hangs_up_during_the_handshake() {
        let (client_io, mut server_io) = duplex(PIPE_CAPACITY);
        let server = tokio::spawn(async move {
            let mut hello = [0; 5];
            server_io.read_exact(&mut hello).await.unwrap();
        });

        let error = connect_failure(client_io, &trusting(), None).await;

        assert!(
            matches!(error, Error::TlsHandshakeError { .. }),
            "{error:?}"
        );
        within(server).await.unwrap();
    }

    #[tokio::test]
    async fn blames_the_transport_when_it_fails() {
        let error = connect_failure(Broken, &trusting(), None).await;

        let Error::TlsHandshakeError { source, .. } = error else {
            panic!("expected a TLS handshake error, got {error:?}");
        };
        let cause = source
            .downcast_ref::<io::Error>()
            .expect("the transport's error");
        assert_eq!(cause.kind(), io::ErrorKind::ConnectionReset);
    }

    #[tokio::test]
    async fn announces_the_handshake_to_the_transport_once() {
        for use_alpn in [true, false] {
            let (client_io, server_io) = duplex(PIPE_CAPACITY);
            let server = serve(server_io, |mut tls| async move {
                let mut received = Vec::new();
                tls.read_to_end(&mut received).await.unwrap();
            });
            let announced = Arc::new(AtomicUsize::new(0));
            let transport = Probe {
                inner: client_io,
                announced: Arc::clone(&announced),
            };

            let validation = TlsValidationConfig {
                use_alpn,
                ..trusting()
            };
            let mut stream = connect(transport, &validation, None).await.unwrap();
            within(stream.shutdown()).await.unwrap();

            assert_eq!(announced.load(Ordering::SeqCst), 1, "use_alpn={use_alpn}");
            within(server).await.unwrap();
        }
    }

    #[tokio::test]
    async fn tunneled_handshakes_are_left_to_native_tls() {
        let (client_io, server_io) = duplex(PIPE_CAPACITY);
        let server = serve(server_io, |mut tls| async move {
            let mut request = [0; 4];
            tls.read_exact(&mut request).await.unwrap();
            tls.write_all(&request).await.unwrap();
            tls.flush().await.unwrap();
            let mut trailing = Vec::new();
            tls.read_to_end(&mut trailing).await.unwrap();
        });

        let validation = TlsValidationConfig {
            use_alpn: false,
            ..trusting()
        };
        let mut stream = connect(client_io, &validation, None).await.unwrap();
        stream.write_all(b"ping").await.unwrap();
        stream.flush().await.unwrap();
        let mut reply = [0; 4];
        within(stream.read_exact(&mut reply)).await.unwrap();
        assert_eq!(&reply, b"ping");

        within(stream.shutdown()).await.unwrap();
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
