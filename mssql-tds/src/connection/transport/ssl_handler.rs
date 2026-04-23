// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use crate::connection::transport::certificate_validator;
use crate::connection::transport::network_transport::Stream;
use crate::io::packet_writer::PacketWriter;
use crate::message::messages::PacketType;
use byteorder::{BigEndian, ByteOrder};
use native_tls::TlsConnector as NativeTlsConnector;
use std::collections::HashMap;
use std::io::{Error, IoSlice};
use std::pin::Pin;
use std::sync::RwLock;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_native_tls::TlsStream;
use tracing::{debug, error, info, warn};

use super::network_transport::PRE_NEGOTIATED_PACKET_SIZE;
use crate::core::{
    EncryptionOptions, EncryptionSetting, NegotiatedEncryptionSetting, TDS_8_ALPN_PROTOCOL,
    TdsResult,
};
#[cfg(target_os = "macos")]
use std::io::{ErrorKind, Write};

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

fn get_or_build_connector(
    validation: &TlsValidationConfig,
) -> Result<NativeTlsConnector, native_tls::Error> {
    if let Some(connector) = CONNECTOR_CACHE.read().unwrap().get(validation) {
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
        .unwrap()
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
                    info!("Validating server certificate using: {}", cert_path);

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
            let mut packet_header_receive_bytes = self.packet_header_receive_bytes.take().unwrap();
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
            let mut write_state = self.write_state.take().unwrap();
            let mut packet_write_buffer = self.packet_write_buffer.take().unwrap();
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
        opts.server_certificate = Some("cert.pem".to_string());
        let config =
            SslHandler::resolve_tls_validation(&opts, NegotiatedEncryptionSetting::Mandatory);
        assert!(config.accept_invalid_certs);
        assert!(config.accept_invalid_hostnames);
        assert!(!config.use_alpn);
    }

    #[test]
    fn server_certificate_takes_precedence_over_login_only() {
        let mut opts = default_options();
        opts.server_certificate = Some("cert.pem".to_string());
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
        opts.server_certificate = Some("cert.pem".to_string());
        let config = SslHandler::resolve_tls_validation(&opts, NegotiatedEncryptionSetting::Strict);
        assert!(config.use_alpn);
        assert!(config.accept_invalid_certs);
        assert!(config.accept_invalid_hostnames);
    }
}
