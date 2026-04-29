// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Extractable stream wrapper for "Login Only" TLS disable.
//!
//! This module provides `ExtractableStream` and `ExtractableStreamHandle` which allow
//! the underlying TCP/Named Pipe stream to be reclaimed after TLS has been enabled.
//! This is needed for "Login Only" encryption mode where TLS is used only during
//! the login phase and then disabled for subsequent communication.

use crate::connection::transport::network_transport::Stream;
use std::io::Error;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// A handle that allows extracting the underlying stream from an ExtractableStream.
/// This is used to disable TLS after the login phase in "Login Only" encryption mode.
#[derive(Clone)]
pub(crate) struct ExtractableStreamHandle {
    inner: Arc<Mutex<Option<Box<dyn Stream>>>>,
}

impl ExtractableStreamHandle {
    /// Create a new extractable stream handle and wrapper.
    /// Returns the handle and the wrapper stream that should be passed to TLS.
    pub(crate) fn new(stream: Box<dyn Stream>) -> (Self, ExtractableStream) {
        let inner = Arc::new(Mutex::new(Some(stream)));
        let handle = ExtractableStreamHandle {
            inner: Arc::clone(&inner),
        };
        let wrapper = ExtractableStream { inner };
        (handle, wrapper)
    }

    /// Extract the underlying stream, consuming it from the wrapper.
    /// This should be called after the TLS stream has been dropped or forgotten.
    /// Returns None if the stream was already extracted.
    pub(crate) fn extract(&self) -> Option<Box<dyn Stream>> {
        self.inner.lock().unwrap().take()
    }
}

/// A wrapper stream that allows the underlying stream to be extracted.
/// This is used to reclaim the TCP stream when disabling TLS for "Login Only" encryption.
pub(crate) struct ExtractableStream {
    inner: Arc<Mutex<Option<Box<dyn Stream>>>>,
}

impl AsyncRead for ExtractableStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let mut guard = self.inner.lock().unwrap();
        if let Some(ref mut stream) = *guard {
            Pin::new(stream.as_mut()).poll_read(cx, buf)
        } else {
            Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "Stream has been extracted",
            )))
        }
    }
}

impl AsyncWrite for ExtractableStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, Error>> {
        let mut guard = self.inner.lock().unwrap();
        if let Some(ref mut stream) = *guard {
            Pin::new(stream.as_mut()).poll_write(cx, buf)
        } else {
            Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "Stream has been extracted",
            )))
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        let mut guard = self.inner.lock().unwrap();
        if let Some(ref mut stream) = *guard {
            Pin::new(stream.as_mut()).poll_flush(cx)
        } else {
            Poll::Ready(Ok(()))
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        let mut guard = self.inner.lock().unwrap();
        if let Some(ref mut stream) = *guard {
            Pin::new(stream.as_mut()).poll_shutdown(cx)
        } else {
            Poll::Ready(Ok(()))
        }
    }
}

impl Stream for ExtractableStream {
    fn tls_handshake_starting(&mut self) {
        if let Some(ref mut stream) = *self.inner.lock().unwrap() {
            stream.tls_handshake_starting();
        }
    }

    fn tls_handshake_completed(&mut self) {
        if let Some(ref mut stream) = *self.inner.lock().unwrap() {
            stream.tls_handshake_completed();
        }
    }

    fn is_connection_dead(&self) -> bool {
        if let Some(ref stream) = *self.inner.lock().unwrap() {
            stream.is_connection_dead()
        } else {
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::ErrorKind;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::io::{AsyncReadExt, AsyncWriteExt, duplex};

    /// A mock stream that implements Stream trait for testing.
    struct MockStream {
        /// The underlying duplex stream for simulating network I/O.
        inner: tokio::io::DuplexStream,
        /// Flag to track if `tls_handshake_starting()` was called.
        handshake_started: Arc<AtomicBool>,
        /// Flag to track if `tls_handshake_completed()` was called.
        handshake_completed: Arc<AtomicBool>,
    }

    impl MockStream {
        fn new() -> (Self, tokio::io::DuplexStream) {
            let (client, server) = duplex(1024);
            (
                MockStream {
                    inner: client,
                    handshake_started: Arc::new(AtomicBool::new(false)),
                    handshake_completed: Arc::new(AtomicBool::new(false)),
                },
                server,
            )
        }
    }

    impl AsyncRead for MockStream {
        fn poll_read(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            Pin::new(&mut self.inner).poll_read(cx, buf)
        }
    }

    impl AsyncWrite for MockStream {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<Result<usize, Error>> {
            Pin::new(&mut self.inner).poll_write(cx, buf)
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
            Pin::new(&mut self.inner).poll_flush(cx)
        }

        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<Result<(), Error>> {
            Pin::new(&mut self.inner).poll_shutdown(cx)
        }
    }

    impl Stream for MockStream {
        fn tls_handshake_starting(&mut self) {
            self.handshake_started.store(true, Ordering::SeqCst);
        }

        fn tls_handshake_completed(&mut self) {
            self.handshake_completed.store(true, Ordering::SeqCst);
        }
    }

    #[tokio::test]
    async fn test_extractable_stream_read_write() {
        let (mock_stream, mut server) = MockStream::new();
        let (handle, mut extractable) = ExtractableStreamHandle::new(Box::new(mock_stream));

        // Write data through extractable stream
        let write_data = b"hello world";
        extractable.write_all(write_data).await.unwrap();

        // Read from server side
        let mut buf = vec![0u8; write_data.len()];
        server.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, write_data);

        // Write from server, read through extractable
        let response = b"response";
        server.write_all(response).await.unwrap();

        let mut buf = vec![0u8; response.len()];
        extractable.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, response);

        // Handle should still have stream (not extracted yet)
        assert!(handle.extract().is_some());
    }

    #[tokio::test]
    async fn test_extract_returns_stream() {
        let (mock_stream, _server) = MockStream::new();
        let (handle, _extractable) = ExtractableStreamHandle::new(Box::new(mock_stream));

        // First extraction should succeed
        let extracted = handle.extract();
        assert!(extracted.is_some());

        // Second extraction should return None
        let extracted_again = handle.extract();
        assert!(extracted_again.is_none());
    }

    #[tokio::test]
    async fn test_read_after_extract_returns_error() {
        let (mock_stream, _server) = MockStream::new();
        let (handle, mut extractable) = ExtractableStreamHandle::new(Box::new(mock_stream));

        // Extract the stream
        let _extracted = handle.extract();

        // Read should fail with NotConnected error
        let mut buf = vec![0u8; 10];
        let result = extractable.read(&mut buf).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), ErrorKind::NotConnected);
    }

    #[tokio::test]
    async fn test_write_after_extract_returns_error() {
        let (mock_stream, _server) = MockStream::new();
        let (handle, mut extractable) = ExtractableStreamHandle::new(Box::new(mock_stream));

        // Extract the stream
        let _extracted = handle.extract();

        // Write should fail with NotConnected error
        let result = extractable.write(b"data").await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), ErrorKind::NotConnected);
    }

    #[tokio::test]
    async fn test_flush_after_extract_succeeds() {
        let (mock_stream, _server) = MockStream::new();
        let (handle, mut extractable) = ExtractableStreamHandle::new(Box::new(mock_stream));

        // Extract the stream
        let _extracted = handle.extract();

        // Flush should succeed (no-op when extracted)
        let result = extractable.flush().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_shutdown_after_extract_succeeds() {
        let (mock_stream, _server) = MockStream::new();
        let (handle, mut extractable) = ExtractableStreamHandle::new(Box::new(mock_stream));

        // Extract the stream
        let _extracted = handle.extract();

        // Shutdown should succeed (no-op when extracted)
        let result = extractable.shutdown().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_tls_handshake_callbacks_propagate() {
        let (mock_stream, _server) = MockStream::new();
        let handshake_started = mock_stream.handshake_started.clone();
        let handshake_completed = mock_stream.handshake_completed.clone();

        let (_handle, mut extractable) = ExtractableStreamHandle::new(Box::new(mock_stream));

        // Callbacks should propagate to underlying stream
        assert!(!handshake_started.load(Ordering::SeqCst));
        assert!(!handshake_completed.load(Ordering::SeqCst));

        extractable.tls_handshake_starting();
        assert!(handshake_started.load(Ordering::SeqCst));
        assert!(!handshake_completed.load(Ordering::SeqCst));

        extractable.tls_handshake_completed();
        assert!(handshake_started.load(Ordering::SeqCst));
        assert!(handshake_completed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_tls_handshake_callbacks_after_extract_are_noop() {
        let (mock_stream, _server) = MockStream::new();
        let handshake_started = mock_stream.handshake_started.clone();
        let handshake_completed = mock_stream.handshake_completed.clone();

        let (handle, mut extractable) = ExtractableStreamHandle::new(Box::new(mock_stream));

        // Extract the stream
        let _extracted = handle.extract();

        // Callbacks should be no-ops (not panic)
        extractable.tls_handshake_starting();
        extractable.tls_handshake_completed();

        // Original flags should still be false (callbacks didn't reach the extracted stream)
        assert!(!handshake_started.load(Ordering::SeqCst));
        assert!(!handshake_completed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_handle_clone_shares_state() {
        let (mock_stream, _server) = MockStream::new();
        let (handle1, _extractable) = ExtractableStreamHandle::new(Box::new(mock_stream));
        let handle2 = handle1.clone();

        // Extract using first handle
        let extracted = handle1.extract();
        assert!(extracted.is_some());

        // Second handle should see the stream as already extracted
        let extracted_again = handle2.extract();
        assert!(extracted_again.is_none());
    }
}
