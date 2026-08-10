// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Blocking buffer-owning reader (sans-I/O core, sync edge).
//!
//! [`BlockingPacketReader`] is the synchronous sibling of
//! [`crate::io::packet_reader::PacketReader`]: it owns the reassembly
//! [`PacketBuffer`] the sync [`TdsCore::step_row`](crate::io::tds_core::TdsCore)
//! body decodes over, and supplies the one thing the buffer cannot do itself —
//! pull more bytes — by *blocking* on a [`BlockingByteSource`] rather than
//! awaiting a socket. It is the reusable shell the L4 `TdsSyncClient` will wire to
//! a real blocking transport; at L3 it is driven only over the in-memory corpus
//! feeder that backs the differential and residency tests, so no live blocking
//! socket exists yet.
//!
//! The refill path reuses the shared, byte-source-agnostic framing body
//! ([`assemble_tds_packet_blocking`]) plus [`PacketBuffer::strip_header`], exactly
//! as the async [`PacketReader`](crate::io::packet_reader::PacketReader) reuses
//! `assemble_tds_packet`. Only the byte-pull edge differs.

use crate::core::TdsResult;
use crate::io::byte_source::{BlockingByteSource, assemble_tds_packet_blocking};
use crate::io::packet_buffer::PacketBuffer;
use crate::io::token_stream::BlockingRowReader;

/// A buffer-owning reader whose refill blocks on a [`BlockingByteSource`].
///
/// Generic (not `dyn`) over the source so the framing body is monomorphized per
/// source, mirroring the async seam. The blocking driver
/// ([`crate::io::token_stream::drive_row_over_buffer_blocking`]) reaches its
/// owned buffer through the [`BlockingRowReader`] impl below.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) struct BlockingPacketReader<S: BlockingByteSource> {
    source: S,
    buffer: PacketBuffer,
}

#[cfg_attr(not(test), allow(dead_code))]
impl<S: BlockingByteSource> BlockingPacketReader<S> {
    /// Creates a reader with a `2 x packet_size` working buffer, matching the
    /// async [`PacketReader`](crate::io::packet_reader::PacketReader) ceiling that
    /// bounds PLP residency.
    pub(crate) fn new(source: S, packet_size: usize) -> Self {
        Self {
            source,
            buffer: PacketBuffer::with_packet_size(packet_size),
        }
    }

    /// Creates a reader seeded with `residual` bytes carried over from an
    /// async→blocking edge flip, so the first decode/refill continues exactly
    /// where the async parser paused (see
    /// [`PacketBuffer::seed_residual`](crate::io::packet_buffer::PacketBuffer::seed_residual)).
    pub(crate) fn with_seeded_buffer(
        source: S,
        packet_size: usize,
        residual: &crate::io::packet_buffer::ResidualBytes,
    ) -> Self {
        let mut buffer = PacketBuffer::with_packet_size(packet_size);
        buffer.seed_residual(residual);
        Self { source, buffer }
    }

    /// Drains the unconsumed bytes for handoff back across a blocking→async flip.
    pub(crate) fn take_residual(&mut self) -> crate::io::packet_buffer::ResidualBytes {
        self.buffer.take_residual()
    }

    /// Consumes the reader and returns its byte source (to recover the owned
    /// socket on revert).
    pub(crate) fn into_source(self) -> S {
        self.source
    }

    /// Mutable access to the byte source, so the owning client can refresh the
    /// per-request cancel/deadline policy before each fetch.
    pub(crate) fn source_mut(&mut self) -> &mut S {
        &mut self.source
    }

    /// Reads one complete TDS packet synchronously and strips its 8-byte header,
    /// exposing the payload. The forward-progress guard lives in
    /// [`BlockingRowReader::refill_row_buffer_blocking`].
    fn read_tds_packet_blocking(&mut self) -> TdsResult<()> {
        let packet_len = assemble_tds_packet_blocking(&mut self.source, &mut self.buffer)?;
        self.buffer.strip_header(packet_len);
        Ok(())
    }
}

#[cfg_attr(not(test), allow(dead_code))]
impl<S: BlockingByteSource> BlockingRowReader for BlockingPacketReader<S> {
    fn row_buffer_mut(&mut self) -> &mut PacketBuffer {
        &mut self.buffer
    }

    fn refill_row_buffer_blocking(&mut self) -> TdsResult<()> {
        let before = self.buffer.available();
        self.read_tds_packet_blocking()?;
        if self.buffer.available() <= before {
            return Err(crate::error::Error::ProtocolError(
                "TDS packet refill made no progress during blocking row decode".to_string(),
            ));
        }
        Ok(())
    }
}
