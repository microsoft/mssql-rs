// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Async wrapper around the sync Schannel handshake state machine.
//!
//! This module is the resolution to plan §0 Bug 2 (the
//! `MidHandshakeTlsStream` + `tokio_native_tls` waker-park bug):
//!
//! - The handshake's `InitializeSecurityContextW` calls are microsecond-scale
//!   CPU work and run **synchronously** on the calling task. No
//!   `spawn_blocking`, because that would waste tokio's bounded blocking
//!   pool, add context switches, and not actually be blocking work.
//! - The only `await` points are explicit `socket.read(...).await` and
//!   `socket.write_all(...).await` calls in the driver loop.
//! - There is exactly one buffer of unconsumed encrypted bytes
//!   (`enc_in: Vec<u8>`). After every `read.await` that returns `Ready(n)`
//!   we unconditionally re-enter `step()` with the now-larger buffer —
//!   there is no `WouldBlock` shim that could trick us into parking the
//!   waker after the final wire byte has arrived.
//!
//! [`SchannelTlsStream::connect`] plus the [`AsyncRead`] / [`AsyncWrite`]
//! surface drive both the handshake and the steady-state (post-handshake)
//! reads/writes through the record layer.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Instant;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tracing::{debug, trace};

use super::cred::{CredHandle, CredKind};
use super::handshake::{Handshake, StepOutcome};
use super::record_layer::{Decrypted, RecordLayer};

/// Internal mode discriminator.
///
/// `Handshaking` is only used during [`SchannelTlsStream::connect`]; by
/// the time a `SchannelTlsStream` is returned to the caller it is always
/// in `Streaming` mode (or `connect` returned an `Err`).
enum Mode {
    /// Post-handshake steady state. Holds the record layer wrapping the
    /// `SecCtx` plus a tail of unconsumed encrypted bytes and an output
    /// queue of plaintext bytes awaiting drainage by `poll_read`.
    Streaming {
        record: RecordLayer,
        enc_in: Vec<u8>,
        /// Decrypted plaintext we've produced but the caller has not yet
        /// drained. Necessary because a single decrypt may yield more
        /// bytes than the caller's `ReadBuf` can hold.
        plain_out: Vec<u8>,
        /// Encrypted record currently being flushed to the socket. While
        /// non-empty, `poll_write` MUST finish draining it before
        /// encrypting the next plaintext chunk — otherwise a new TLS
        /// record would be appended mid-stream and corrupt the wire
        /// (server responds with RST). Paired with `pending_plain_len`
        /// which records how many plaintext bytes this record represents
        /// so we can return the correct `Ok(n)` once fully sent.
        pending_out: Vec<u8>,
        pending_out_written: usize,
        pending_plain_len: usize,
    },
}

/// Async TLS stream wrapping an arbitrary `AsyncRead + AsyncWrite` socket.
///
/// Construct via [`SchannelTlsStream::connect`]. The returned stream
/// implements [`AsyncRead`] / [`AsyncWrite`] for transparent encrypted I/O:
/// `poll_read` decrypts incoming TLS records and `poll_write` encrypts
/// outgoing plaintext, so callers see a plain byte stream while the record
/// layer handles framing, buffering of partial records, and back-pressure.
pub(crate) struct SchannelTlsStream<S> {
    socket: S,
    mode: Mode,
    /// Cached `tls-unique` channel binding token (full `SEC_CHANNEL_BINDINGS`
    /// blob) captured at the end of the handshake. `None` when extraction was
    /// suppressed or failed. Surfaced via [`SchannelTlsStream::channel_binding_token`].
    channel_binding: Option<Vec<u8>>,
}

impl<S> SchannelTlsStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    /// Drive a TLS client handshake to completion against `socket`.
    ///
    /// The loop is a literal transcription of plan §2.3:
    ///
    /// ```text
    /// loop {
    ///     let outcome = handshake.step(&mut enc_in, &mut consumed)?;
    ///     if consumed > 0 { enc_in.drain(..consumed); }
    ///     match outcome {
    ///         Done | DoneWithFlush => return Ok(...),
    ///         WantWriteThenRead(out) => socket.write_all(&out).await?,
    ///         NeedMoreInput => { /* fall through to read */ }
    ///     }
    ///     let n = socket.read(&mut tmp).await?;
    ///     if n == 0 { return Err(unexpected EOF); }
    ///     enc_in.extend_from_slice(&tmp[..n]);
    /// }
    /// ```
    ///
    /// Crucially: a `Ready(n)` return from `socket.read(...).await` is
    /// followed by an unconditional re-entry into `step()`. There is no
    /// path through this driver that parks a waker after bytes have been
    /// delivered to `enc_in`.
    pub(crate) async fn connect(
        mut socket: S,
        cred: Arc<CredHandle>,
        kind: CredKind,
        server_name: &str,
        alpn_blob: Option<Vec<u8>>,
    ) -> io::Result<Self> {
        let cred_for_record = Arc::clone(&cred);
        let alpn_requested = alpn_blob.is_some();
        let mut handshake = Handshake::new(cred, kind, server_name, alpn_blob);
        let mut enc_in: Vec<u8> = Vec::new();
        let mut read_buf = vec![0u8; 8192];
        let connect_started = Instant::now();
        let mut read_count: u32 = 0;
        let mut write_count: u32 = 0;
        let mut bytes_read: usize = 0;
        let mut bytes_written: usize = 0;

        loop {
            let mut consumed = 0;
            let outcome = handshake.step(enc_in.as_mut_slice(), &mut consumed)?;
            if consumed > 0 {
                enc_in.drain(..consumed);
            }

            match outcome {
                StepOutcome::Done { sizes } => {
                    // `enc_in` already retains the SECBUFFER_EXTRA tail after
                    // draining `consumed` bytes above. Do NOT re-append the
                    // `extra` copy — that would duplicate the first post-
                    // handshake bytes (e.g. a TLS 1.3 NewSessionTicket
                    // piggybacked on the final flight) and the record layer
                    // would fail to decrypt the duplicated copy.
                    log_negotiated_alpn(handshake.ctx_handle(), alpn_requested);
                    let ctx = handshake.into_ctx();
                    // Capture the tls-unique channel binding token before the
                    // context is moved into the record layer.
                    let channel_binding = extract_channel_binding(&ctx);
                    debug!(
                        elapsed_ms = connect_started.elapsed().as_millis() as u64,
                        socket_reads = read_count,
                        socket_writes = write_count,
                        bytes_read,
                        bytes_written,
                        extra_buffered = enc_in.len(),
                        "win_tls: stream entering Streaming mode (no flush needed)"
                    );
                    return Ok(SchannelTlsStream {
                        socket,
                        mode: Mode::Streaming {
                            record: RecordLayer::new(
                                ctx,
                                sizes,
                                cred_for_record,
                                kind,
                                server_name,
                            ),
                            enc_in,
                            plain_out: Vec::new(),
                            pending_out: Vec::new(),
                            pending_out_written: 0,
                            pending_plain_len: 0,
                        },
                        channel_binding,
                    });
                }
                StepOutcome::DoneWithFlush { out, sizes } => {
                    socket.write_all(&out).await?;
                    socket.flush().await?;
                    write_count += 1;
                    bytes_written += out.len();
                    // See Done branch: `enc_in` already retains the
                    // SECBUFFER_EXTRA tail; do not re-append `extra`.
                    log_negotiated_alpn(handshake.ctx_handle(), alpn_requested);
                    let ctx = handshake.into_ctx();
                    // Capture the tls-unique channel binding token before the
                    // context is moved into the record layer.
                    let channel_binding = extract_channel_binding(&ctx);
                    debug!(
                        elapsed_ms = connect_started.elapsed().as_millis() as u64,
                        socket_reads = read_count,
                        socket_writes = write_count,
                        bytes_read,
                        bytes_written,
                        extra_buffered = enc_in.len(),
                        "win_tls: stream entering Streaming mode (after final flush)"
                    );
                    return Ok(SchannelTlsStream {
                        socket,
                        mode: Mode::Streaming {
                            record: RecordLayer::new(
                                ctx,
                                sizes,
                                cred_for_record,
                                kind,
                                server_name,
                            ),
                            enc_in,
                            plain_out: Vec::new(),
                            pending_out: Vec::new(),
                            pending_out_written: 0,
                            pending_plain_len: 0,
                        },
                        channel_binding,
                    });
                }
                StepOutcome::WantWriteThenRead(out) => {
                    socket.write_all(&out).await?;
                    socket.flush().await?;
                    write_count += 1;
                    bytes_written += out.len();
                    // After write, we still need more bytes to continue.
                    // Fall through to the read below.
                }
                StepOutcome::NeedMoreInput => {
                    // SChannel needs more wire bytes. Fall through to read.
                }
            }

            let n = socket.read(&mut read_buf).await?;
            if n == 0 {
                debug!(
                    elapsed_ms = connect_started.elapsed().as_millis() as u64,
                    socket_reads = read_count,
                    socket_writes = write_count,
                    bytes_read,
                    bytes_written,
                    "win_tls: peer closed during handshake (EOF)"
                );
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "peer closed connection during TLS handshake",
                ));
            }
            read_count += 1;
            bytes_read += n;
            trace!(
                read_n = n,
                bytes_read_total = bytes_read,
                "win_tls: socket read"
            );
            enc_in.extend_from_slice(&read_buf[..n]);
        }
    }

    /// Reference to the inner socket. Useful for callers that need to
    /// reach through (e.g. propagate connection-state callbacks).
    pub(crate) fn get_ref(&self) -> &S {
        &self.socket
    }

    /// Mutable reference to the inner socket. Same caveats as `get_ref`.
    pub(crate) fn get_mut(&mut self) -> &mut S {
        &mut self.socket
    }

    /// Reference to the active SChannel security context.
    pub(crate) fn ctx(&self) -> &super::handshake::SecCtx {
        match &self.mode {
            Mode::Streaming { record, .. } => record.ctx(),
        }
    }

    /// The `tls-unique` channel binding token (full `SEC_CHANNEL_BINDINGS`
    /// blob) captured at handshake completion, if extraction succeeded.
    pub(crate) fn channel_binding_token(&self) -> Option<Vec<u8>> {
        self.channel_binding.clone()
    }
}

/// Extract the `tls-unique` channel binding token from a freshly completed
/// security context.
///
/// Logs and returns `None` on failure rather than aborting the connection:
/// a missing channel binding only matters when the server enforces Extended
/// Protection, in which case the login itself will fail with a clear server
/// error. Mirrors the non-fatal ALPN-query handling above.
fn extract_channel_binding(ctx: &super::handshake::SecCtx) -> Option<Vec<u8>> {
    match super::bindings::query_unique_bindings(ctx) {
        Ok(token) => {
            debug!(
                token_len = token.len(),
                "win_tls: extracted tls-unique channel binding token"
            );
            Some(token)
        }
        Err(e) => {
            debug!(error = %e, "win_tls: failed to extract channel binding token");
            None
        }
    }
}

/// Query and log the ALPN protocol the server selected after a successful
/// handshake. Matches the `native-tls` engine's behaviour: log only, never
/// fail. `requested` differentiates "we didn't ask" from "we asked and
/// the server didn't pick".
fn log_negotiated_alpn(
    ctx: &windows_sys::Win32::Security::Credentials::SecHandle,
    requested: bool,
) {
    if !requested {
        return;
    }
    match super::alpn::query_negotiated_alpn(ctx) {
        Ok(Some(proto)) => {
            debug!(
                proto = %super::alpn::debug_proto(&proto),
                "win_tls: server negotiated ALPN protocol"
            );
        }
        Ok(None) => {
            debug!("win_tls: server did not negotiate an ALPN protocol");
        }
        Err(e) => {
            debug!(error = %e, "win_tls: failed to query negotiated ALPN");
        }
    }
}

// --- AsyncRead / AsyncWrite using the record layer ---

/// Decide what a zero-byte socket read means for `poll_read`.
///
/// A clean EOF is only graceful when there is no buffered ciphertext. If
/// `enc_in` still holds bytes, the peer closed in the middle of a TLS
/// record: those bytes can never form a complete record, so we surface an
/// `UnexpectedEof` instead of masking the truncation as a graceful close
/// (which would hand the TDS layer a silently short read).
fn read_eof_outcome(enc_in_empty: bool) -> Poll<io::Result<()>> {
    if enc_in_empty {
        Poll::Ready(Ok(()))
    } else {
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "win_tls: connection closed in the middle of a TLS record",
        )))
    }
}

impl<S> AsyncRead for SchannelTlsStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let Mode::Streaming {
            record,
            enc_in,
            plain_out,
            ..
        } = &mut this.mode;

        loop {
            // 1. Drain any buffered plaintext first.
            if !plain_out.is_empty() {
                let n = std::cmp::min(plain_out.len(), buf.remaining());
                buf.put_slice(&plain_out[..n]);
                plain_out.drain(..n);
                return Poll::Ready(Ok(()));
            }

            // 2. Try to decrypt one record out of enc_in.
            match record.decrypt(enc_in, plain_out) {
                Ok(Decrypted::Ok) => continue, // back to step 1 to drain
                Ok(Decrypted::PeerClosed) => return Poll::Ready(Ok(())),
                Ok(Decrypted::NeedMoreInput) => {
                    // 3. Need more bytes from the wire.
                }
                Err(e) => return Poll::Ready(Err(e)),
            }

            // 3. Read more from the socket.
            let mut tmp = [0u8; 8192];
            let mut tmp_buf = ReadBuf::new(&mut tmp);
            match Pin::new(&mut this.socket).poll_read(cx, &mut tmp_buf) {
                Poll::Ready(Ok(())) => {
                    let filled = tmp_buf.filled().len();
                    if filled == 0 {
                        return read_eof_outcome(enc_in.is_empty());
                    }
                    enc_in.extend_from_slice(&tmp[..filled]);
                    // Loop and try decrypt again.
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl<S> AsyncWrite for SchannelTlsStream<S>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let this = self.get_mut();
        let Mode::Streaming {
            record,
            pending_out,
            pending_out_written,
            pending_plain_len,
            ..
        } = &mut this.mode;

        // If a previous poll_write left an encrypted record only partially
        // sent, finish flushing THAT record before encrypting anything new.
        // Sending a fresh record into the middle of an old one corrupts
        // the stream and the peer RSTs (Windows error 10054).
        if !pending_out.is_empty() {
            while *pending_out_written < pending_out.len() {
                match Pin::new(&mut this.socket)
                    .poll_write(cx, &pending_out[*pending_out_written..])
                {
                    Poll::Ready(Ok(0)) => {
                        return Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::WriteZero,
                            "socket accepted zero bytes",
                        )));
                    }
                    Poll::Ready(Ok(n)) => *pending_out_written += n,
                    Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                    Poll::Pending => return Poll::Pending,
                }
            }
            let plain_len = *pending_plain_len;
            // INVARIANT: a pending record is only ever stashed after we
            // returned `Poll::Pending` without advancing the caller's
            // plaintext cursor, so the next poll_write is expected to pass
            // the same buffer (this is exactly how `AsyncWriteExt::write_all`
            // drives us). `plain_len` is therefore the count of leading bytes
            // of `buf` that this drained record represents. Returning a value
            // greater than `buf.len()` would violate the AsyncWrite contract;
            // assert the invariant in debug builds to catch any caller that
            // retries with a shorter/different buffer.
            debug_assert!(
                plain_len <= buf.len(),
                "win_tls: poll_write drained a pending record of {plain_len} \
                 plaintext bytes but the retry buffer is only {} bytes; the \
                 caller must retry with the same buffer after Poll::Pending",
                buf.len()
            );
            trace!(
                drained = pending_out.len(),
                plain_len, "win_tls: poll_write drained pending record"
            );
            pending_out.clear();
            *pending_out_written = 0;
            *pending_plain_len = 0;
            return Poll::Ready(Ok(plain_len));
        }

        let chunk = std::cmp::min(buf.len(), record.max_message());
        let mut out = Vec::with_capacity(record.header_len() + chunk + record.trailer_len());
        if let Err(e) = record.encrypt(&buf[..chunk], &mut out) {
            return Poll::Ready(Err(e));
        }

        let mut written = 0;
        while written < out.len() {
            match Pin::new(&mut this.socket).poll_write(cx, &out[written..]) {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "socket accepted zero bytes",
                    )));
                }
                Poll::Ready(Ok(n)) => {
                    written += n;
                }
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => {
                    // Stash the unsent tail of the encrypted record. The
                    // next poll_write (called by the caller's write_all
                    // loop) will drain it before touching any new
                    // plaintext. We deliberately return Pending here even
                    // when we made some progress: AsyncWrite contract
                    // allows it, and reporting a partial `Ok(n < chunk)`
                    // would let the caller advance its plaintext cursor
                    // past bytes we still hold for retransmission.
                    debug!(
                        encrypted_total = out.len(),
                        written_so_far = written,
                        plain_chunk = chunk,
                        "win_tls: poll_write partial socket write, stashing pending record"
                    );
                    *pending_out = out;
                    *pending_out_written = written;
                    *pending_plain_len = chunk;
                    return Poll::Pending;
                }
            }
        }
        Poll::Ready(Ok(chunk))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let Mode::Streaming {
            pending_out,
            pending_out_written,
            pending_plain_len,
            ..
        } = &mut this.mode;

        while *pending_out_written < pending_out.len() {
            match Pin::new(&mut this.socket).poll_write(cx, &pending_out[*pending_out_written..]) {
                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "socket accepted zero bytes",
                    )));
                }
                Poll::Ready(Ok(n)) => *pending_out_written += n,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            }
        }
        if !pending_out.is_empty() {
            pending_out.clear();
            *pending_out_written = 0;
            *pending_plain_len = 0;
        }
        Pin::new(&mut this.socket).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut this.socket).poll_shutdown(cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    /// Verify the async driver returns UnexpectedEof when the server hangs
    /// up after consuming the ClientHello. Exercises the Ready(0) branch
    /// of the read loop without needing a real TLS server.
    #[tokio::test]
    async fn handshake_returns_eof_when_peer_closes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            // Accept and immediately drop — peer sees ClientHello then EOF.
            let (sock, _) = listener.accept().await.unwrap();
            // Read what they send so the client's write_all completes,
            // then drop.
            let sock = sock;
            let mut tmp = vec![0u8; 8192];
            let _ = sock.readable().await;
            let _ = sock.try_read(&mut tmp);
            drop(sock);
        });

        let client_sock = tokio::net::TcpStream::connect(addr).await.unwrap();
        let cred = super::super::cred::get_or_acquire(CredKind::NoValidate).unwrap();
        let result =
            SchannelTlsStream::connect(client_sock, cred, CredKind::NoValidate, "127.0.0.1", None)
                .await;
        assert!(result.is_err(), "expected handshake to fail on EOF");
        let err = match result {
            Ok(_) => unreachable!("checked above"),
            Err(e) => e,
        };
        // Either UnexpectedEof from our driver or some SChannel error after
        // SChannel parses zero bytes. Either way the call must NOT hang.
        assert!(
            err.kind() == io::ErrorKind::UnexpectedEof
                || err.to_string().contains("InitializeSecurityContextW")
                || err.kind() == io::ErrorKind::ConnectionReset,
            "unexpected error: {err:?}"
        );

        server.await.unwrap();
    }

    // --- Pure decision / steady-state plumbing (no live TLS server) ---

    use super::super::handshake::SecCtx;
    use windows_sys::Win32::Security::Authentication::Identity;

    #[test]
    fn read_eof_outcome_clean_close_on_empty_buffer() {
        match read_eof_outcome(true) {
            Poll::Ready(Ok(())) => {}
            other => panic!("expected graceful EOF, got {other:?}"),
        }
    }

    #[test]
    fn read_eof_outcome_truncated_record_is_error() {
        match read_eof_outcome(false) {
            Poll::Ready(Err(e)) => assert_eq!(e.kind(), io::ErrorKind::UnexpectedEof),
            other => panic!("expected UnexpectedEof, got {other:?}"),
        }
    }

    /// In-memory socket that accepts every write and reports EOF on read.
    struct MockSocket {
        written: Vec<u8>,
    }

    impl AsyncRead for MockSocket {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for MockSocket {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            self.written.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    fn streaming_stream(
        socket: MockSocket,
        pending_out: Vec<u8>,
        pending_plain_len: usize,
    ) -> SchannelTlsStream<MockSocket> {
        let cred = super::super::cred::get_or_acquire(CredKind::NoValidate).unwrap();
        // SAFETY: SecPkgContext_StreamSizes is a plain POD struct; the
        // record layer isn't exercised by the pending-drain path under test.
        let sizes: Identity::SecPkgContext_StreamSizes = unsafe { std::mem::zeroed() };
        let record = RecordLayer::new(
            SecCtx::for_test_only(),
            sizes,
            cred,
            CredKind::NoValidate,
            "test",
        );
        SchannelTlsStream {
            socket,
            mode: Mode::Streaming {
                record,
                enc_in: Vec::new(),
                plain_out: Vec::new(),
                pending_out,
                pending_out_written: 0,
                pending_plain_len,
            },
            channel_binding: None,
        }
    }

    #[test]
    fn poll_write_drains_pending_record_and_returns_plain_len() {
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        let mut s = streaming_stream(
            MockSocket {
                written: Vec::new(),
            },
            vec![10, 20, 30, 40],
            7,
        );
        let buf = [0u8; 16];

        match Pin::new(&mut s).poll_write(&mut cx, &buf) {
            Poll::Ready(Ok(n)) => assert_eq!(n, 7, "must report the stashed plaintext length"),
            other => panic!("expected Ready(Ok(7)), got {other:?}"),
        }

        let Mode::Streaming {
            pending_out,
            pending_out_written,
            pending_plain_len,
            ..
        } = &s.mode;
        assert!(pending_out.is_empty(), "pending record must be cleared");
        assert_eq!(*pending_out_written, 0);
        assert_eq!(*pending_plain_len, 0);
        assert_eq!(
            s.socket.written,
            vec![10, 20, 30, 40],
            "record forwarded to socket"
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "retry buffer is only")]
    fn poll_write_pending_drain_asserts_buffer_invariant() {
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        // pending_plain_len (100) exceeds the retry buffer (4): a caller that
        // violated the same-buffer invariant must trip the debug assert
        // rather than silently returning Ok(n > buf.len()).
        let mut s = streaming_stream(
            MockSocket {
                written: Vec::new(),
            },
            vec![1, 2, 3, 4],
            100,
        );
        let buf = [0u8; 4];
        let _ = Pin::new(&mut s).poll_write(&mut cx, &buf);
    }

    #[test]
    fn poll_read_drains_buffered_plaintext_first() {
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        let mut s = streaming_stream(
            MockSocket {
                written: Vec::new(),
            },
            Vec::new(),
            0,
        );
        let Mode::Streaming { plain_out, .. } = &mut s.mode;
        plain_out.extend_from_slice(&[1, 2, 3, 4, 5]);
        let mut backing = [0u8; 3];
        let mut rb = ReadBuf::new(&mut backing);
        match Pin::new(&mut s).poll_read(&mut cx, &mut rb) {
            Poll::Ready(Ok(())) => assert_eq!(rb.filled(), &[1, 2, 3]),
            other => panic!("expected Ready(Ok), got {other:?}"),
        }
        let Mode::Streaming { plain_out, .. } = &s.mode;
        assert_eq!(plain_out, &vec![4, 5], "leftover plaintext stays buffered");
    }

    #[test]
    fn poll_read_returns_graceful_eof_when_socket_closes() {
        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        // Empty enc_in makes decrypt short-circuit to NeedMoreInput (no FFI),
        // then the MockSocket reports EOF and the loop closes gracefully.
        let mut s = streaming_stream(
            MockSocket {
                written: Vec::new(),
            },
            Vec::new(),
            0,
        );
        let mut backing = [0u8; 16];
        let mut rb = ReadBuf::new(&mut backing);
        match Pin::new(&mut s).poll_read(&mut cx, &mut rb) {
            Poll::Ready(Ok(())) => assert_eq!(rb.filled().len(), 0, "clean EOF yields zero bytes"),
            other => panic!("expected graceful EOF, got {other:?}"),
        }
    }
}
