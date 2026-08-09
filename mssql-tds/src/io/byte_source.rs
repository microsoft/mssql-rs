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

use std::time::Duration;

use async_trait::async_trait;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;
use tracing::event;

use crate::connection::transport::network_transport::Stream;
use crate::core::{CancelHandle, TdsResult};
use crate::error::Error::TimeoutError;
use crate::error::TimeoutErrorType;
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

/// The receive-edge timeout+cancel policy for one in-flight request.
///
/// The async shell carries the request's total deadline and cancel signal here
/// and applies them at the socket-read edge ([`AsyncByteSource::receive`]),
/// instead of wrapping the whole parse future in a tokio timer. That keeps the
/// shared parse/pump body ([`assemble_tds_packet`] and the token/row drivers)
/// timer- and cancel-agnostic — a future non-tokio driver supplies its own edge
/// policy. The deadline is absolute, snapshotted once when the drive begins, so
/// the many socket reads of a single request all share one total-request
/// deadline rather than restarting a per-read clock.
#[derive(Clone, Default)]
pub(crate) struct ReceiveGuard {
    /// Absolute instant the in-flight request must complete by. `None` waits
    /// indefinitely, matching the no-timeout arm of the pre-lift shell.
    deadline: Option<tokio::time::Instant>,
    /// Cooperative cancel, cloned from the caller's `CancelHandle`; a clone
    /// observes the same cancellation as the original.
    cancel: Option<CancellationToken>,
}

impl ReceiveGuard {
    pub(crate) fn new(
        remaining_request_timeout: Option<Duration>,
        cancel_handle: Option<&CancelHandle>,
    ) -> Self {
        Self {
            deadline: remaining_request_timeout.map(|d| tokio::time::Instant::now() + d),
            cancel: cancel_handle.map(|h| h.cancel_token.clone()),
        }
    }
}

/// The single production byte source: the live socket. Reads straight off the
/// stream and records a broken connection so the cached liveness check reports
/// it dead, exactly as the transport's inline packet read did before the seam.
///
/// The socket read is wrapped by the request's [`ReceiveGuard`] so timeout and
/// cancellation fire at this edge, producing the same error kinds the async
/// shell's whole-future tokio wrap did before the lift.
pub(crate) struct AsyncByteSource<'a> {
    stream: &'a mut dyn Stream,
    known_dead: &'a mut bool,
    guard: ReceiveGuard,
}

impl<'a> AsyncByteSource<'a> {
    pub(crate) fn new(
        stream: &'a mut dyn Stream,
        known_dead: &'a mut bool,
        guard: ReceiveGuard,
    ) -> Self {
        Self {
            stream,
            known_dead,
            guard,
        }
    }
}

#[async_trait]
impl ByteSource for AsyncByteSource<'_> {
    async fn receive(&mut self, buffer: &mut [u8]) -> TdsResult<usize> {
        let deadline = self.guard.deadline;
        let cancel = self.guard.cancel.clone();
        let known_dead = &mut *self.known_dead;
        let stream = &mut *self.stream;

        let read = async move {
            match stream.read(buffer).await {
                Ok(0) => {
                    // EOF mid-read: the peer closed the connection. Record it so
                    // the cached liveness check reports the connection as dead;
                    // the caller turns the `0` into a `ConnectionClosed` error.
                    *known_dead = true;
                    Ok(0)
                }
                Ok(n) => Ok(n),
                Err(e) => {
                    // A read failure means the socket is broken; record it so the
                    // cached liveness check reports the connection as dead.
                    *known_dead = true;
                    Err(e.into())
                }
            }
        };

        // Cancel is inner and the deadline outer, matching the pre-lift
        // `timeout(t, run_until_cancelled(cancel, ..))` nesting: a ready cancel
        // wins over a simultaneously-elapsed deadline. On either, the read
        // future is dropped without completing, so `known_dead` stays untouched.
        let cancel_handle = cancel.map(CancelHandle::from);
        let guarded = CancelHandle::run_until_cancelled(cancel_handle.as_ref(), read);
        match deadline {
            Some(deadline) => match tokio::time::timeout_at(deadline, guarded).await {
                Ok(result) => result,
                Err(elapsed) => Err(TimeoutError(TimeoutErrorType::Elapsed(elapsed))),
            },
            None => guarded.await,
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
