// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Sans-I/O packet buffer: the synchronous, I/O-free half of packet reading.
//!
//! [`PacketBuffer`] owns the reassembled TDS payload bytes and serves every
//! scalar and byte read straight from memory. It knows nothing about sockets or
//! `async`. The only thing it cannot do itself is obtain more bytes; when a read
//! needs more than [`available`](PacketBuffer::available), the caller (a thin
//! I/O shell) refills the buffer via [`begin_refill`](PacketBuffer::begin_refill)
//! / [`commit_packet`](PacketBuffer::commit_packet) and retries.
//!
//! This is the foundation of the sans-I/O split: protocol decode is pure
//! computation over this buffer, and the `.await` lives only at the refill edge.

use byteorder::{BigEndian, ByteOrder, LittleEndian};

use crate::core::TdsResult;
use crate::io::packet_writer::PacketWriter;

/// Synchronous, I/O-free buffer of reassembled TDS packet payload bytes.
pub(crate) struct PacketBuffer {
    working_buffer: Vec<u8>,
    position: usize,
    length: usize,
    max_packet_size: usize,
}

impl PacketBuffer {
    /// Creates a buffer sized to hold two packets, matching the historic
    /// `PacketReader` storage so a value that straddles a packet boundary still
    /// fits after a single refill.
    #[cfg(test)]
    pub(crate) fn with_packet_size(max_packet_size: usize) -> Self {
        PacketBuffer {
            working_buffer: vec![0; max_packet_size * 2],
            position: 0,
            length: 0,
            max_packet_size,
        }
    }

    /// Bytes currently readable without a refill.
    pub(crate) fn available(&self) -> usize {
        self.length - self.position
    }

    /// Whether `byte_count` bytes can be read without a refill.
    pub(crate) fn has(&self, byte_count: usize) -> bool {
        self.available() >= byte_count
    }

    /// Readable bytes as a slice (from the current position to the filled end).
    fn peek(&self) -> &[u8] {
        &self.working_buffer[self.position..self.length]
    }

    /// Returns the first `n` readable bytes, erroring if fewer are buffered.
    ///
    /// Callers ensure enough bytes are present (via a refill) before calling; a
    /// shortfall here is a protocol/logic error, not a request for more data.
    fn take(&mut self, n: usize) -> TdsResult<&[u8]> {
        if !self.has(n) {
            return Err(crate::error::Error::ProtocolError(format!(
                "Buffer underflow: needed {} bytes but only {} available",
                n,
                self.available()
            )));
        }
        let start = self.position;
        self.position += n;
        if self.position == self.length {
            self.position = 0;
            self.length = 0;
        }
        Ok(&self.working_buffer[start..start + n])
    }

    pub(crate) fn take_u8(&mut self) -> TdsResult<u8> {
        Ok(self.take(1)?[0])
    }

    pub(crate) fn take_i16_be(&mut self) -> TdsResult<i16> {
        Ok(BigEndian::read_i16(self.take(2)?))
    }

    pub(crate) fn take_i32_be(&mut self) -> TdsResult<i32> {
        Ok(BigEndian::read_i32(self.take(4)?))
    }

    pub(crate) fn take_uint40_le(&mut self) -> TdsResult<u64> {
        Ok(LittleEndian::read_uint(self.take(5)?, 5))
    }

    pub(crate) fn take_f32_le(&mut self) -> TdsResult<f32> {
        Ok(LittleEndian::read_f32(self.take(4)?))
    }

    pub(crate) fn take_f64_le(&mut self) -> TdsResult<f64> {
        Ok(LittleEndian::read_f64(self.take(8)?))
    }

    pub(crate) fn take_i16_le(&mut self) -> TdsResult<i16> {
        Ok(LittleEndian::read_i16(self.take(2)?))
    }

    pub(crate) fn take_u16_le(&mut self) -> TdsResult<u16> {
        Ok(LittleEndian::read_u16(self.take(2)?))
    }

    pub(crate) fn take_u24_le(&mut self) -> TdsResult<u32> {
        Ok(LittleEndian::read_u24(self.take(3)?))
    }

    pub(crate) fn take_i32_le(&mut self) -> TdsResult<i32> {
        Ok(LittleEndian::read_i32(self.take(4)?))
    }

    pub(crate) fn take_u32_le(&mut self) -> TdsResult<u32> {
        Ok(LittleEndian::read_u32(self.take(4)?))
    }

    pub(crate) fn take_i64_le(&mut self) -> TdsResult<i64> {
        Ok(LittleEndian::read_i64(self.take(8)?))
    }

    pub(crate) fn take_u64_le(&mut self) -> TdsResult<u64> {
        Ok(LittleEndian::read_u64(self.take(8)?))
    }

    /// Copies up to `dst.len()` readable bytes into `dst`, consuming them, and
    /// returns how many were copied (bounded by what is currently buffered).
    pub(crate) fn copy_out(&mut self, dst: &mut [u8]) -> usize {
        let to_copy = dst.len().min(self.available());
        if to_copy > 0 {
            dst[..to_copy].copy_from_slice(&self.peek()[..to_copy]);
            self.position += to_copy;
            if self.position == self.length {
                self.position = 0;
                self.length = 0;
            }
        }
        to_copy
    }

    /// Discards up to `count` readable bytes, returning how many were skipped
    /// (bounded by what is currently buffered).
    pub(crate) fn skip_available(&mut self, count: usize) -> usize {
        let to_skip = count.min(self.available());
        self.position += to_skip;
        if self.position == self.length {
            self.position = 0;
            self.length = 0;
        }
        to_skip
    }

    /// Compacts leftover bytes to the front and returns the offset at which the
    /// next raw packet (header included) should be written. Pair with
    /// [`commit_packet`](Self::commit_packet) once the packet has been read.
    pub(crate) fn begin_refill(&mut self) -> usize {
        let remaining = self.available();
        if remaining > 0 {
            self.working_buffer
                .copy_within(self.position..self.length, 0);
        }
        self.length = remaining;
        self.position = 0;
        self.length
    }

    /// The window a raw packet is read into, starting `already` bytes past
    /// `base`. The receive edge fills this incrementally until a full packet is
    /// present.
    pub(crate) fn refill_window(&mut self, base: usize, already: usize) -> &mut [u8] {
        &mut self.working_buffer[base + already..base + self.max_packet_size]
    }

    /// Strips the 8-byte packet header from a raw packet written at `base` and
    /// makes its `raw_len - 8` payload bytes readable.
    pub(crate) fn commit_packet(&mut self, base: usize, raw_len: usize) {
        self.working_buffer.copy_within(
            base + PacketWriter::PACKET_HEADER_SIZE..base + raw_len,
            base,
        );
        self.length = base + raw_len - PacketWriter::PACKET_HEADER_SIZE;
    }

    /// Big-endian packet length recorded in the header of the raw packet at
    /// `base` (bytes 2..4 of the TDS packet header).
    pub(crate) fn packet_header_length(&self, base: usize) -> usize {
        BigEndian::read_u16(&self.working_buffer[base + 2..base + 4]) as usize
    }

    /// Debug view of the raw bytes read for the packet at `base`.
    pub(crate) fn raw_packet(&self, base: usize, raw_len: usize) -> &[u8] {
        &self.working_buffer[base..base + raw_len]
    }

    /// True once every buffered byte has been consumed.
    pub(crate) fn is_drained(&self) -> bool {
        self.position == self.length
    }
}
