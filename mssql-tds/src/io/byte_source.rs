// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! The `ByteSource` seam: the single "give me more bytes" edge that TDS packet
//! assembly pulls through.
//!
//! Packet framing ([`assemble_tds_packet`]) is byte-source-agnostic — it only
//! ever needs one primitive from the outside world: pull some more bytes into a
//! caller-owned window. Formalizing that edge as [`ByteSource`] lets the one
//! assembly body run over either the live socket ([`AsyncByteSource`]) or any
//! [`NetworkReader`] (the buffer-owning test reader) without duplicating the
//! header/payload loops. The seam is generic, not `dyn`: callers pass a concrete
//! source and the body is monomorphized per source.

use async_trait::async_trait;
use tokio::io::AsyncReadExt;
use tracing::event;

use crate::connection::transport::network_transport::Stream;
use crate::core::TdsResult;
use crate::io::packet_buffer::PacketBuffer;
use crate::io::packet_writer::PacketWriter;
use crate::io::reader_writer::{NetworkReader, NetworkReaderWriter};

/// The sole primitive TDS packet assembly needs from the outside world.
///
/// Pull up to `buffer.len()` more bytes into `buffer`, returning how many were
/// read. A return of `0` means the peer has no more bytes (EOF); the assembly
/// body turns that into a [`ConnectionClosed`](crate::error::Error::ConnectionClosed).
#[async_trait]
pub(crate) trait ByteSource: Send {
    async fn receive(&mut self, buffer: &mut [u8]) -> TdsResult<usize>;
}

/// Bridges the buffer-owning test reader onto the seam: a `NetworkReader` is
/// already a byte pull, so forward it unchanged and share the one assembly body.
#[async_trait]
impl<'a> ByteSource for dyn NetworkReaderWriter + 'a {
    async fn receive(&mut self, buffer: &mut [u8]) -> TdsResult<usize> {
        NetworkReader::receive(self, buffer).await
    }
}

/// The single production byte source: the live socket. Reads straight off the
/// stream and records a broken connection so the cached liveness check reports
/// it dead, exactly as the transport's inline packet read did before the seam.
pub(crate) struct AsyncByteSource<'a> {
    stream: &'a mut dyn Stream,
    known_dead: &'a mut bool,
}

impl<'a> AsyncByteSource<'a> {
    pub(crate) fn new(stream: &'a mut dyn Stream, known_dead: &'a mut bool) -> Self {
        Self { stream, known_dead }
    }
}

#[async_trait]
impl ByteSource for AsyncByteSource<'_> {
    async fn receive(&mut self, buffer: &mut [u8]) -> TdsResult<usize> {
        match self.stream.read(buffer).await {
            Ok(0) => {
                // EOF mid-read: the peer closed the connection. Record it so the
                // cached liveness check reports the connection as dead; the
                // caller turns the `0` into a `ConnectionClosed` error.
                *self.known_dead = true;
                Ok(0)
            }
            Ok(n) => Ok(n),
            Err(e) => {
                // A read failure means the socket is broken; record it so the
                // cached liveness check reports the connection as dead.
                *self.known_dead = true;
                Err(e.into())
            }
        }
    }
}

/// Reads one complete TDS packet from `source` into `buffer` and returns that
/// packet's declared length (header + payload).
///
/// The single packet-framing body, pulling through the [`ByteSource`] seam. The
/// 8-byte header may arrive split across reads, so it keeps reading until the
/// header is complete before trusting its declared length. A single read may
/// pull bytes past this packet (TCP coalescing, Named Pipes message mode); the
/// surplus is carried forward via `record_pending` so the next refill strips its
/// header too.
pub(crate) async fn assemble_tds_packet<S: ByteSource + ?Sized>(
    source: &mut S,
    buffer: &mut PacketBuffer,
) -> TdsResult<usize> {
    // Compact readable bytes and replay any pending bytes from a prior
    // multi-packet read; `base` is where this raw packet begins and `already`
    // is how many of its bytes are already buffered.
    let (base, already) = buffer.begin_refill()?;
    let mut received = already;

    while received < PacketWriter::PACKET_HEADER_SIZE {
        let bytes_read = source.receive(buffer.refill_window(base, received)).await?;
        if bytes_read == 0 {
            return Err(crate::error::Error::ConnectionClosed(
                "Connection closed by server while reading TDS packet header".to_string(),
            ));
        }
        received += bytes_read;
    }

    // Validate the declared length (>= header, <= max packet size, fits in the
    // buffer) before trusting it to bound the payload read.
    let packet_size_from_header = buffer.validate_packet_length(base)?;
    while received < packet_size_from_header {
        let bytes_read = source.receive(buffer.refill_window(base, received)).await?;
        if bytes_read == 0 {
            return Err(crate::error::Error::ConnectionClosed(
                "Connection closed by server while reading TDS packet payload".to_string(),
            ));
        }
        received += bytes_read;
    }

    // Bytes read past this packet belong to the next one; carry them forward.
    buffer.record_pending(base, packet_size_from_header, received);

    event!(
        tracing::Level::DEBUG,
        "Received packet of size: {:?}",
        packet_size_from_header
    );

    use pretty_hex::PrettyHex;
    event!(
        tracing::Level::DEBUG,
        "Packet content: {:?}",
        buffer.raw_packet(base, packet_size_from_header).hex_dump()
    );

    Ok(packet_size_from_header)
}
