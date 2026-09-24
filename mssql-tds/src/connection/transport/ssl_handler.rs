// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::connection::transport::network_transport::Stream;
use crate::connection::transport::tls::{
    CustomRoots, TlsConnectParams, TlsValidationConfig, default_engine,
};
use crate::io::packet_writer::PacketWriter;
use crate::message::messages::PacketType;
use byteorder::{BigEndian, ByteOrder};
use std::io::{Error, IoSlice};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tracing::{debug, error, info};

use super::network_transport::PRE_NEGOTIATED_PACKET_SIZE;
use crate::core::{
    EncryptionOptions, NegotiatedEncryptionSetting, ServerTrust, TdsResult, TrustRoots,
};
#[cfg(target_os = "macos")]
use std::io::{ErrorKind, Write};

#[derive(Debug)]
pub(crate) struct SslHandler {
    pub(crate) server_host_name: String,
    /// Boxed to keep `NetworkTransport` (and the futures holding it) small.
    pub(crate) encryption_options: Box<EncryptionOptions>,
}

impl SslHandler {
    /// Determine TLS certificate validation behavior based on encryption options
    /// and the negotiated encryption mode.
    pub(crate) fn resolve_tls_validation(
        encryption_options: &EncryptionOptions,
        negotiated_encryption: NegotiatedEncryptionSetting,
    ) -> TlsValidationConfig {
        let use_alpn = negotiated_encryption == NegotiatedEncryptionSetting::Strict;
        let (accept_invalid_certs, accept_invalid_hostnames, custom_roots) =
            match &encryption_options.server_trust {
                // Pinning bypasses chain validation; the DER match runs after the handshake.
                ServerTrust::Pinned(_) => (true, true, None),
                ServerTrust::DangerAcceptAny => (true, false, None),
                ServerTrust::Verify { roots, .. } => match roots {
                    // ODBC parity: login-only encryption does not validate platform trust.
                    TrustRoots::Platform => (
                        negotiated_encryption == NegotiatedEncryptionSetting::LoginOnly,
                        false,
                        None,
                    ),
                    // Explicit trust anchors are always enforced.
                    TrustRoots::Custom(source) => (
                        false,
                        false,
                        Some(CustomRoots {
                            source: source.clone(),
                            include_platform_roots: false,
                        }),
                    ),
                    TrustRoots::PlatformAndCustom(source) => (
                        false,
                        false,
                        Some(CustomRoots {
                            source: source.clone(),
                            include_platform_roots: true,
                        }),
                    ),
                },
            };
        TlsValidationConfig {
            accept_invalid_certs,
            accept_invalid_hostnames,
            use_alpn,
            custom_roots,
        }
    }

    pub(crate) async fn enable_ssl_async(
        &self,
        base_stream: Box<dyn Stream>,
        negotiated_encryption: NegotiatedEncryptionSetting,
    ) -> TdsResult<Box<dyn Stream>> {
        let validation =
            Self::resolve_tls_validation(&self.encryption_options, negotiated_encryption);

        let host_name = match &self.encryption_options.server_trust {
            ServerTrust::Verify {
                host_name: Some(host_name),
                ..
            } if !host_name.is_empty() => host_name.as_str(),
            _ => self.server_host_name.as_str(),
        };

        info!(
            "TLS config: encryption_mode={:?}, server_trust={:?}, resolved_host_name={}, server_host_name={}",
            self.encryption_options.mode,
            self.encryption_options.server_trust,
            host_name,
            self.server_host_name,
        );

        let pinned_certificate = match &self.encryption_options.server_trust {
            ServerTrust::Pinned(source) => Some(source),
            _ => None,
        };

        let params = TlsConnectParams {
            validation: &validation,
            host_name,
            server_host_name: &self.server_host_name,
            pinned_certificate,
        };

        default_engine(&validation)
            .connect(base_stream, params)
            .await
    }
}

struct ActiveWriteState {
    header_bytes_remaining: usize,
    payload_bytes_remaining: usize,
    current_packet_bytes_remaining: usize,
    packet_id: u8,
    last_payload_written: usize,
}

impl ActiveWriteState {
    const PRELOGIN_MAX_PACKET_SIZE: usize =
        PRE_NEGOTIATED_PACKET_SIZE as usize - PacketWriter::PACKET_HEADER_SIZE;
    const MAX_PACKET_SIZE_WITHOUT_HEADER: usize =
        Self::PRELOGIN_MAX_PACKET_SIZE - PacketWriter::PACKET_HEADER_SIZE;

    fn new() -> Self {
        ActiveWriteState {
            header_bytes_remaining: PacketWriter::PACKET_HEADER_SIZE,
            payload_bytes_remaining: 0,
            current_packet_bytes_remaining: 0,
            packet_id: 0,
            last_payload_written: 0,
        }
    }

    fn start_next_packet(&mut self, new_payload_len: usize) {
        // This should either be the first packet or the continuation of the last payload.
        assert!(
            new_payload_len == self.payload_bytes_remaining || self.payload_bytes_remaining == 0
        );
        self.header_bytes_remaining = PacketWriter::PACKET_HEADER_SIZE;
        self.payload_bytes_remaining = new_payload_len;
        self.last_payload_written = 0;
        self.current_packet_bytes_remaining =
            std::cmp::min(Self::MAX_PACKET_SIZE_WITHOUT_HEADER, new_payload_len);
        self.packet_id = self.packet_id.wrapping_add(1);
    }

    // Returns the number of bytes in the external payload that got written.
    fn on_successful_write(&mut self, bytes_written: usize) -> usize {
        let mut payload_written = bytes_written;
        if self.header_bytes_remaining > 0 {
            if bytes_written <= self.header_bytes_remaining {
                self.header_bytes_remaining -= bytes_written;
                self.last_payload_written = 0;
                0
            } else {
                // At least one byte from the payload was written.
                payload_written -= self.header_bytes_remaining;
                self.header_bytes_remaining = 0;

                // Ensure we don't write more payload than the current packet can hold
                let actual_payload_written =
                    std::cmp::min(payload_written, self.current_packet_bytes_remaining);
                self.current_packet_bytes_remaining -= actual_payload_written;
                self.payload_bytes_remaining -= actual_payload_written;
                self.last_payload_written = actual_payload_written;
                actual_payload_written
            }
        } else {
            payload_written
        }
    }

    fn setup_prelogin_packet_header(&self, buf: &mut Vec<u8>) {
        buf.clear();
        let _ = PacketWriter::build_header(
            buf,
            self.current_packet_bytes_remaining + PacketWriter::PACKET_HEADER_SIZE,
            PacketType::PreLogin,
            self.packet_id,
            self.current_packet_bytes_remaining == self.payload_bytes_remaining,
            false,
            crate::message::messages::ResetConnectionMode::None,
        );
    }
}

pub(crate) struct TlsOverTdsStream<S: Stream> {
    wrapped_stream: S,
    has_completed_tls_handshake: bool,
    remaining_read_packet_payload_length: usize,
    packet_header_receive_bytes: Option<[u8; PacketWriter::PACKET_HEADER_SIZE]>,
    bytes_of_packet_header_read: usize,
    packet_write_buffer: Option<Vec<u8>>,
    write_state: Option<ActiveWriteState>,
}

impl<S: Stream> TlsOverTdsStream<S> {
    pub(crate) fn new(wrapped_stream: S) -> Self {
        Self::new_with_handshake_state(wrapped_stream, true)
    }

    fn new_with_handshake_state(wrapped_stream: S, has_completed_tls_handshake: bool) -> Self {
        TlsOverTdsStream {
            wrapped_stream,
            has_completed_tls_handshake,
            remaining_read_packet_payload_length: 0,
            packet_header_receive_bytes: Some([0; PacketWriter::PACKET_HEADER_SIZE]),
            bytes_of_packet_header_read: 0,
            packet_write_buffer: Some(vec![0; PacketWriter::PACKET_HEADER_SIZE]),
            write_state: None,
        }
    }

    fn read_requested(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let wanted_count =
            std::cmp::min(buf.remaining(), self.remaining_read_packet_payload_length);
        let mut read_buffer = buf.take(wanted_count);
        let result = AsyncRead::poll_read(Pin::new(&mut self.wrapped_stream), cx, &mut read_buffer);
        match result {
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Ready(Ok(())) => {
                if read_buffer.filled().is_empty() {
                    // Report EOF to caller.
                    error!("Got EOF reading payload");
                    Poll::Ready(Ok(()))
                } else {
                    let length_read = read_buffer.filled().len();
                    debug!("Payload bytes read: {:?}", length_read);
                    self.remaining_read_packet_payload_length -= length_read;
                    buf.advance(length_read);
                    Poll::Ready(Ok(()))
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<S: Stream> AsyncRead for TlsOverTdsStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.has_completed_tls_handshake {
            AsyncRead::poll_read(Pin::new(&mut self.wrapped_stream), cx, buf)
        } else if self.remaining_read_packet_payload_length > 0 {
            self.read_requested(cx, buf)
        } else {
            // Read a new packet, starting with the header.
            let mut packet_header_receive_bytes = match self.packet_header_receive_bytes.take() {
                Some(packet_header_receive_bytes) => packet_header_receive_bytes,
                None => {
                    return Poll::Ready(Err(std::io::Error::other(
                        "TLS packet header buffer missing",
                    )));
                }
            };
            let external_res = loop {
                // This might be a continuation of reading the header. An earlier loop iteration may have only
                // partially retrieved the header. This function also may have returned pending.
                // Use the bytes_of_packet_header_read field to pick up where we left off.
                assert!(
                    self.remaining_read_packet_payload_length < PacketWriter::PACKET_HEADER_SIZE
                );

                // Try to read the length of the header, potentially in chunks. If the read call returns Ok,
                // but read_buffer.remaining() > 0, there's more of the header to read.
                let mut read_buffer = ReadBuf::new(
                    &mut packet_header_receive_bytes[self.remaining_read_packet_payload_length..],
                );
                let header_read_result =
                    AsyncRead::poll_read(Pin::new(&mut self.wrapped_stream), cx, &mut read_buffer);

                match header_read_result {
                    Poll::Ready(Err(e)) => {
                        error!(
                            "Read error on wrapped_stream (named pipe): {:?}, full error: {:?}",
                            e.kind(),
                            e
                        );
                        break Poll::Ready(Err(e));
                    }
                    Poll::Pending => {
                        break Poll::Pending;
                    }
                    Poll::Ready(Ok(())) => {
                        debug!("Read bytes read {:?}", read_buffer.filled().len());
                        if read_buffer.filled().is_empty() {
                            // Report EOF to caller.
                            error!("Got EOF reading header");
                            break Poll::Ready(Ok(()));
                        } else if read_buffer.remaining() > 0 {
                            // Update the cached read_buffer and returning pending so the caller will
                            // try to get more of the header.
                            debug!("Header bytes read {:?}", read_buffer.filled().len());
                            self.bytes_of_packet_header_read -= read_buffer.filled().len();
                            continue;
                        } else {
                            // The whole header should have been read exactly.
                            debug!("Header fully read");
                            assert_eq!(
                                read_buffer.filled().len() + self.bytes_of_packet_header_read,
                                PacketWriter::PACKET_HEADER_SIZE
                            );

                            // Got the whole packet header. Reset the packet header byte read counter.
                            self.bytes_of_packet_header_read = 0;

                            // Get the packet size from the header and store it for possibly subsequent calls.
                            self.remaining_read_packet_payload_length =
                                BigEndian::read_u16(&packet_header_receive_bytes[2..4]) as usize
                                    - PacketWriter::PACKET_HEADER_SIZE;

                            // Also return ownership of the packet header buffer so that it can be used for the next packet.
                            self.packet_header_receive_bytes = Some(packet_header_receive_bytes);
                            break self.as_mut().read_requested(cx, buf);
                        }
                    }
                }
            };

            self.packet_header_receive_bytes = Some(packet_header_receive_bytes);
            external_res
        }
    }
}

impl<S: Stream> AsyncWrite for TlsOverTdsStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, Error>> {
        debug!("poll_write() called.");
        if self.has_completed_tls_handshake {
            AsyncWrite::poll_write(Pin::new(&mut self.wrapped_stream), cx, buf)
        } else {
            debug!("poll_write() calling poll_write_vectored() internally");
            AsyncWrite::poll_write_vectored(Pin::new(&mut self), cx, &[IoSlice::new(buf)])
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        debug!("poll_flush() called.");
        AsyncWrite::poll_flush(Pin::new(&mut self.wrapped_stream), cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        debug!("poll_shutdown() called");
        AsyncWrite::poll_shutdown(Pin::new(&mut self.wrapped_stream), cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<Result<usize, Error>> {
        debug!("poll_write_vectored() called");
        if self.has_completed_tls_handshake {
            AsyncWrite::poll_write_vectored(Pin::new(&mut self.wrapped_stream), cx, bufs)
        } else {
            // This is a loop because we have to keep writing to the internal stream until
            // at least one payload byte is written or the payload stream returns pending.
            if self.write_state.is_none() {
                self.write_state = Some(ActiveWriteState::new());
            }

            let payload_len = bufs.iter().map(|b| b.len()).sum::<usize>();
            let mut write_state = match self.write_state.take() {
                Some(write_state) => write_state,
                None => {
                    return Poll::Ready(Err(std::io::Error::other("TLS write state missing")));
                }
            };
            let mut packet_write_buffer = match self.packet_write_buffer.take() {
                Some(packet_write_buffer) => packet_write_buffer,
                None => {
                    self.write_state = Some(write_state);
                    return Poll::Ready(Err(std::io::Error::other(
                        "TLS packet write buffer missing",
                    )));
                }
            };
            let external_res = loop {
                // The supplied buffer needs to be wrapped in TDS packets and may need to be split
                // across multiple packets (so multiple writes).
                let needs_new_packet = write_state.current_packet_bytes_remaining == 0;
                if needs_new_packet {
                    write_state.start_next_packet(payload_len);
                    write_state.setup_prelogin_packet_header(&mut packet_write_buffer);
                }

                let needs_to_send_header = write_state.header_bytes_remaining > 0;

                // Use poll_write_vectored to avoid having to manually concatenate buffers. If the stream
                // is optimized for this (eg stream.is_write_vectored returns true), this will zero-copy
                // each buffer. If not, it will internally concatenate buffers for us.
                // Note that poll_write_vectored writes atomically. It is not allowed to partially write and then return Pending/Error.
                let mut slices = Vec::new();
                if needs_to_send_header {
                    let header_start_pos =
                        PacketWriter::PACKET_HEADER_SIZE - write_state.header_bytes_remaining;
                    slices.push(IoSlice::new(&packet_write_buffer[header_start_pos..]));
                    slices.append(&mut Vec::from(bufs));
                } else {
                    // Find the correct position within the series of buffers to resume from.
                    let mut payload_starting_offset = write_state.last_payload_written;
                    for buf in bufs {
                        if payload_starting_offset > buf.len() {
                            // Skip over buf. It was fully consumed.
                            payload_starting_offset -= buf.len();
                        } else {
                            // This buf is partially written. Start from the remaining offset.
                            slices.push(IoSlice::new(&buf[payload_starting_offset..]));
                            // By setting this to zero, all subsequent buffers are fully added to the
                            // write request.
                            payload_starting_offset = 0;
                        }
                    }
                }

                // For named pipes in Message mode, vectored I/O may write header and body separately,
                // causing the 8-byte TDS header to be treated as a complete message, which makes
                // SQL Server close the pipe. Solution: Always flatten multiple slices into a single
                // buffer during TLS handshake to ensure atomic writes.
                let internal_result = if slices.len() > 1 {
                    // Flatten all slices into a single buffer for atomic write
                    let total_len: usize = slices.iter().map(|s| s.len()).sum();
                    let mut flattened = Vec::with_capacity(total_len);
                    for slice in &slices {
                        flattened.extend_from_slice(slice);
                    }
                    debug!(
                        "Flattening {} slices into single buffer of {} bytes for atomic write",
                        slices.len(),
                        total_len
                    );
                    // Write the flattened buffer as a single atomic write
                    AsyncWrite::poll_write(Pin::new(&mut self.wrapped_stream), cx, &flattened)
                } else {
                    AsyncWrite::poll_write_vectored(Pin::new(&mut self.wrapped_stream), cx, &slices)
                };

                match internal_result {
                    Poll::Pending => {
                        debug!("Write pending.");
                        break Poll::Pending;
                    }
                    Poll::Ready(Err(e)) => {
                        error!("Write error {:?}", e.kind());
                        break Poll::Ready(Err(e));
                    }
                    Poll::Ready(Ok(bytes_written)) => {
                        debug!("Bytes written {:?}", bytes_written);
                        // Log first few bytes for debugging TLS handshake
                        if !slices.is_empty() && !slices[0].is_empty() {
                            let preview = &slices[0][..std::cmp::min(16, slices[0].len())];
                            debug!("Write data preview (first 16 bytes): {:02X?}", preview);
                        }
                        if bytes_written == 0 {
                            // Notify EOF to caller.
                            error!("EOF on write.");
                            break Poll::Ready(Ok(bytes_written));
                        }

                        let payload_bytes_written = write_state.on_successful_write(bytes_written);
                        if payload_bytes_written == 0 {
                            // Only header bytes got written. Don't tell the caller 0 got written for
                            // the payload because they'll think they got an EOF.
                            // Continue the loop to retry writing.
                            continue;
                        } else {
                            break Poll::Ready(Ok(payload_bytes_written));
                        }
                    }
                };
            };

            self.write_state = Some(write_state);
            self.packet_write_buffer = Some(packet_write_buffer);
            external_res
        }
    }

    fn is_write_vectored(&self) -> bool {
        // Check if the underlying stream supports vectored writes
        // For named pipes on Windows, vectored writes may not work correctly
        // with TLS handshake, so delegate to the wrapped stream's capability
        debug!("is_write_vectored called");
        self.wrapped_stream.is_write_vectored()
    }
}

impl<S: Stream> Stream for TlsOverTdsStream<S> {
    fn tls_handshake_starting(&mut self) {
        self.has_completed_tls_handshake = false;
        self.wrapped_stream.tls_handshake_starting();
    }

    fn tls_handshake_completed(&mut self) {
        self.has_completed_tls_handshake = true;
        self.wrapped_stream.tls_handshake_completed();
    }

    fn is_connection_dead(&self) -> bool {
        self.wrapped_stream.is_connection_dead()
    }
}

#[cfg(target_os = "macos")]
impl Stream for BufferedTdsStream {
    fn tls_handshake_starting(&mut self) {
        self.is_executing_tls_handshake = true;
        self.tls_over_tds_stream.tls_handshake_starting();
    }

    fn tls_handshake_completed(&mut self) {
        self.is_executing_tls_handshake = false;
        self.tls_over_tds_stream.tls_handshake_completed();
    }

    fn is_connection_dead(&self) -> bool {
        self.tls_over_tds_stream.is_connection_dead()
    }
}

#[cfg(target_os = "macos")]
pub(crate) struct BufferedTdsStream {
    buffer: Option<Vec<u8>>,
    tls_over_tds_stream: TlsOverTdsStream<Box<dyn Stream>>,
    is_executing_tls_handshake: bool,
    buffer_pos: usize,
}

#[cfg(target_os = "macos")]
impl BufferedTdsStream {
    pub(crate) fn new(tls_over_tds_stream: TlsOverTdsStream<Box<dyn Stream>>) -> Self {
        BufferedTdsStream {
            buffer: Some(Vec::with_capacity(
                ActiveWriteState::MAX_PACKET_SIZE_WITHOUT_HEADER,
            )),
            tls_over_tds_stream,
            is_executing_tls_handshake: false,
            buffer_pos: 0,
        }
    }

    fn flush_buffered(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        if !self.buffer.as_ref().unwrap().is_empty() {
            let mut payload = self.buffer.take();

            let res = loop {
                match AsyncWrite::poll_write(
                    Pin::new(&mut self.tls_over_tds_stream),
                    cx,
                    &payload.as_ref().unwrap()[0..],
                ) {
                    Poll::Pending => break Poll::Pending,
                    Poll::Ready(Err(e)) => break Poll::Ready(Err(e)),
                    Poll::Ready(Ok(0)) => {
                        break Poll::Ready(Err(std::io::Error::new(
                            ErrorKind::UnexpectedEof,
                            "eof",
                        )));
                    }
                    Poll::Ready(Ok(bytes_written)) => {
                        self.buffer_pos += bytes_written;
                        if self.buffer_pos == payload.as_ref().unwrap().len() {
                            payload.as_mut().unwrap().clear();
                            self.buffer_pos = 0;
                            break Poll::Ready(Ok(()));
                        } else {
                            continue;
                        }
                    }
                }
            };
            self.buffer = payload.take();
            res
        } else {
            Poll::Ready(Ok(()))
        }
    }
}

#[cfg(target_os = "macos")]
impl AsyncRead for BufferedTdsStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if !self.is_executing_tls_handshake {
            AsyncRead::poll_read(Pin::new(&mut self.tls_over_tds_stream), cx, buf)
        } else {
            match Self::flush_buffered(Pin::new(&mut self), cx) {
                Poll::Pending => Poll::Pending,
                Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
                Poll::Ready(Ok(())) => {
                    AsyncRead::poll_read(Pin::new(&mut self.tls_over_tds_stream), cx, buf)
                }
            }
        }
    }
}

#[cfg(target_os = "macos")]
impl AsyncWrite for BufferedTdsStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, Error>> {
        if !self.is_executing_tls_handshake {
            AsyncWrite::poll_write(Pin::new(&mut self.tls_over_tds_stream), cx, buf)
        } else {
            let _ = Write::write(&mut self.buffer.as_mut().unwrap(), buf);
            Poll::Ready(Ok(buf.len()))
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        if !self.is_executing_tls_handshake {
            AsyncWrite::poll_flush(Pin::new(&mut self.tls_over_tds_stream), cx)
        } else {
            // During TLS handshake, suppress flushes. Security.framework calls
            // write+flush for each TLS record (ClientKeyExchange, ChangeCipherSpec,
            // Finished), but SQL Server expects the entire client flight as a single
            // TDS message. Deferring the flush to poll_read (via flush_buffered)
            // batches all records into one TDS packet.
            Poll::Ready(Ok(()))
        }
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        AsyncWrite::poll_shutdown(Pin::new(&mut self.tls_over_tds_stream), cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<Result<usize, Error>> {
        if !self.is_executing_tls_handshake {
            AsyncWrite::poll_write_vectored(Pin::new(&mut self.tls_over_tds_stream), cx, bufs)
        } else {
            let write_res = Write::write_vectored(&mut self.buffer.as_mut().unwrap(), bufs);
            Poll::Ready(write_res)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{CertificateSource, EncryptionSetting};

    fn options(server_trust: ServerTrust) -> EncryptionOptions {
        EncryptionOptions::new()
            .with_mode(EncryptionSetting::Required)
            .with_server_trust(server_trust)
    }

    fn resolve(
        server_trust: ServerTrust,
        negotiated: NegotiatedEncryptionSetting,
    ) -> TlsValidationConfig {
        SslHandler::resolve_tls_validation(&options(server_trust), negotiated)
    }

    fn ca_file() -> CertificateSource {
        CertificateSource::File("ca.pem".into())
    }

    #[test]
    fn login_only_skips_platform_validation() {
        let config = resolve(
            ServerTrust::default(),
            NegotiatedEncryptionSetting::LoginOnly,
        );
        assert!(config.accept_invalid_certs);
        assert!(!config.accept_invalid_hostnames);
        assert!(!config.use_alpn);
    }

    #[test]
    fn platform_verification_is_enforced_outside_login_only() {
        for negotiated in [
            NegotiatedEncryptionSetting::Mandatory,
            NegotiatedEncryptionSetting::NoEncryption,
            NegotiatedEncryptionSetting::Strict,
        ] {
            let config = resolve(ServerTrust::default(), negotiated);
            assert!(!config.accept_invalid_certs, "{negotiated:?}");
            assert!(!config.accept_invalid_hostnames, "{negotiated:?}");
            assert_eq!(config.custom_roots, None);
        }
    }

    #[test]
    fn accept_any_skips_cert_validation_in_every_mode() {
        for negotiated in [
            NegotiatedEncryptionSetting::Mandatory,
            NegotiatedEncryptionSetting::LoginOnly,
            NegotiatedEncryptionSetting::Strict,
        ] {
            let config = resolve(ServerTrust::DangerAcceptAny, negotiated);
            assert!(config.accept_invalid_certs, "{negotiated:?}");
            assert!(!config.accept_invalid_hostnames, "{negotiated:?}");
        }
    }

    #[test]
    fn pinning_bypasses_chain_validation_in_every_mode() {
        for negotiated in [
            NegotiatedEncryptionSetting::Mandatory,
            NegotiatedEncryptionSetting::LoginOnly,
            NegotiatedEncryptionSetting::Strict,
        ] {
            let config = resolve(ServerTrust::Pinned(ca_file()), negotiated);
            assert!(config.accept_invalid_certs, "{negotiated:?}");
            assert!(config.accept_invalid_hostnames, "{negotiated:?}");
            assert_eq!(config.custom_roots, None);
        }
    }

    #[test]
    fn strict_enables_alpn() {
        for trust in [ServerTrust::default(), ServerTrust::Pinned(ca_file())] {
            assert!(resolve(trust, NegotiatedEncryptionSetting::Strict).use_alpn);
        }
    }

    #[test]
    fn non_strict_modes_disable_alpn() {
        for mode in [
            NegotiatedEncryptionSetting::Mandatory,
            NegotiatedEncryptionSetting::LoginOnly,
            NegotiatedEncryptionSetting::NoEncryption,
        ] {
            let config = resolve(ServerTrust::default(), mode);
            assert!(!config.use_alpn, "use_alpn should be false for {:?}", mode);
        }
    }

    #[test]
    fn custom_roots_keep_full_validation_including_login_only() {
        for (roots, include_platform_roots) in [
            (TrustRoots::Custom(ca_file()), false),
            (TrustRoots::PlatformAndCustom(ca_file()), true),
        ] {
            for negotiated in [
                NegotiatedEncryptionSetting::Mandatory,
                NegotiatedEncryptionSetting::LoginOnly,
            ] {
                let config = resolve(ServerTrust::verify(roots.clone()), negotiated);
                assert!(!config.accept_invalid_certs);
                assert!(!config.accept_invalid_hostnames);
                assert_eq!(
                    config.custom_roots,
                    Some(CustomRoots {
                        source: ca_file(),
                        include_platform_roots,
                    })
                );
            }
        }
    }
}
