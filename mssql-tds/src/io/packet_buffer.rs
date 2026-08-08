// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

//! Sans-I/O packet buffer: the synchronous, I/O-free half of packet reading.
//!
//! [`PacketBuffer`] owns the reassembled TDS payload bytes and serves every
//! scalar and byte read straight from memory. It knows nothing about sockets or
//! `async`. The only thing it cannot do itself is obtain more bytes; when a read
//! needs more than [`available`](PacketBuffer::available), the caller (a thin
//! I/O shell) refills the buffer via [`begin_refill`](PacketBuffer::begin_refill)
//! / [`strip_header`](PacketBuffer::strip_header) and retries.
//!
//! A single socket read can return more than one TDS packet (TCP coalescing,
//! Named Pipes message boundaries). The surplus is tracked as *pending bytes*
//! and replayed on the next refill instead of being re-read from the network.
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
    /// Bytes read from the socket that belong to the *next* packet, carried
    /// forward from a read that returned more than one packet.
    pending_bytes: usize,
    /// Absolute offset of the pending bytes within `working_buffer`.
    pending_bytes_offset: usize,
}

impl PacketBuffer {
    /// Creates a buffer sized to hold two packets, so a value that straddles a
    /// packet boundary still fits after a single refill.
    pub(crate) fn with_packet_size(max_packet_size: usize) -> Self {
        PacketBuffer {
            working_buffer: vec![0; max_packet_size * 2],
            position: 0,
            length: 0,
            max_packet_size,
            pending_bytes: 0,
            pending_bytes_offset: 0,
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

    /// Compacts the readable bytes and any carried pending bytes to the front,
    /// returning `(base, already)`: `base` is where the next raw packet begins
    /// and `already` is how many of its bytes are already buffered from a prior
    /// multi-packet read. Pair with [`strip_header`](Self::strip_header) once a
    /// full packet has been read.
    ///
    /// Errors if the recorded pending region falls outside the buffer, guarding
    /// against corrupted or malicious packet lengths on the wire.
    pub(crate) fn begin_refill(&mut self) -> TdsResult<(usize, usize)> {
        let remaining = self.available();
        if remaining > 0 && self.position != 0 {
            self.working_buffer
                .copy_within(self.position..self.length, 0);
        }
        self.position = 0;
        self.length = remaining;

        let already = self.pending_bytes;
        if already > 0 {
            let src = self.pending_bytes_offset;
            let src_end = src.saturating_add(already);
            let dest_end = remaining.saturating_add(already);
            let buffer_len = self.working_buffer.len();
            if src_end > buffer_len || dest_end > buffer_len {
                return Err(crate::error::Error::ProtocolError(format!(
                    "Invalid pending bytes range: src {src}..{src_end}, dest {remaining}, buffer_len {buffer_len}"
                )));
            }
            self.working_buffer.copy_within(src..src_end, remaining);
        }
        self.pending_bytes = 0;
        self.pending_bytes_offset = 0;
        Ok((remaining, already))
    }

    /// The window a raw packet is read into, starting `already` bytes past
    /// `base` and running to the end of the buffer. The wide window lets a
    /// single socket read pull more than one packet, whose surplus is later
    /// recorded via [`record_pending`](Self::record_pending).
    pub(crate) fn refill_window(&mut self, base: usize, already: usize) -> &mut [u8] {
        &mut self.working_buffer[base + already..]
    }

    /// Big-endian packet length recorded in the header of the raw packet at
    /// `base` (bytes 2..4 of the TDS packet header).
    pub(crate) fn packet_header_length(&self, base: usize) -> usize {
        BigEndian::read_u16(&self.working_buffer[base + 2..base + 4]) as usize
    }

    /// Reads and validates the TDS packet header at `base`, returning the
    /// declared packet length (header + payload). Errors on lengths below the
    /// header size, above the negotiated max packet size, or that would
    /// overflow the buffer.
    pub(crate) fn validate_packet_length(&self, base: usize) -> TdsResult<usize> {
        let packet_len = self.packet_header_length(base);
        if packet_len < PacketWriter::PACKET_HEADER_SIZE {
            return Err(crate::error::Error::ProtocolError(format!(
                "Invalid TDS packet length {}: must be at least {} bytes (header size)",
                packet_len,
                PacketWriter::PACKET_HEADER_SIZE
            )));
        }
        if packet_len > self.max_packet_size {
            return Err(crate::error::Error::ProtocolError(format!(
                "TDS packet length {} exceeds negotiated max packet size {}",
                packet_len, self.max_packet_size
            )));
        }
        let buffer_len = self.working_buffer.len();
        if base.saturating_add(packet_len) > buffer_len {
            return Err(crate::error::Error::ProtocolError(format!(
                "TDS packet length {packet_len} at offset {base} exceeds buffer capacity {buffer_len}"
            )));
        }
        Ok(packet_len)
    }

    /// Records the `received - packet_len` bytes read past this packet as
    /// pending for the next [`begin_refill`](Self::begin_refill).
    pub(crate) fn record_pending(&mut self, base: usize, packet_len: usize, received: usize) {
        let extra = received - packet_len;
        if extra > 0 {
            self.pending_bytes = extra;
            self.pending_bytes_offset = base + packet_len;
        } else {
            self.pending_bytes = 0;
            self.pending_bytes_offset = 0;
        }
    }

    /// Strips the 8-byte header from the packet at the current fill point,
    /// making its `packet_len - 8` payload bytes readable.
    pub(crate) fn strip_header(&mut self, packet_len: usize) {
        let base = self.length;
        self.working_buffer.copy_within(
            base + PacketWriter::PACKET_HEADER_SIZE..base + packet_len,
            base,
        );
        self.length = base + packet_len - PacketWriter::PACKET_HEADER_SIZE;
    }

    /// Resizes the buffer when the negotiated packet size changes, discarding
    /// any buffered state. A no-op when the size is unchanged so in-flight data
    /// and read position survive a redundant call.
    pub(crate) fn change_packet_size(&mut self, packet_size: u32) {
        let packet_size = packet_size as usize;
        if packet_size != self.max_packet_size {
            self.max_packet_size = packet_size;
            self.working_buffer.resize(packet_size * 2, 0);
            self.position = 0;
            self.length = 0;
            self.pending_bytes = 0;
            self.pending_bytes_offset = 0;
        }
    }

    /// Rewinds to the front of the buffer and sets the filled length, preserving
    /// any carried pending bytes.
    pub(crate) fn reset_to_length(&mut self, length: usize) {
        self.position = 0;
        self.length = length;
    }

    /// Debug view of the raw bytes read for the packet at `base`.
    pub(crate) fn raw_packet(&self, base: usize, raw_len: usize) -> &[u8] {
        &self.working_buffer[base..base + raw_len]
    }

    /// True once every buffered byte has been consumed.
    pub(crate) fn is_drained(&self) -> bool {
        self.position == self.length
    }

    #[cfg(test)]
    pub(crate) fn working_buffer(&self) -> &[u8] {
        &self.working_buffer
    }

    #[cfg(test)]
    pub(crate) fn position(&self) -> usize {
        self.position
    }

    #[cfg(test)]
    pub(crate) fn length(&self) -> usize {
        self.length
    }

    #[cfg(test)]
    pub(crate) fn pending_bytes(&self) -> usize {
        self.pending_bytes
    }

    #[cfg(test)]
    pub(crate) fn max_packet_size(&self) -> usize {
        self.max_packet_size
    }

    /// Test-only injection of pending-byte bookkeeping to exercise the refill
    /// bounds guards without staging a real multi-packet socket read.
    #[cfg(test)]
    pub(crate) fn set_pending_for_test(
        &mut self,
        pending_bytes: usize,
        pending_bytes_offset: usize,
    ) {
        self.pending_bytes = pending_bytes;
        self.pending_bytes_offset = pending_bytes_offset;
    }

    #[cfg(test)]
    pub(crate) fn working_buffer_mut(&mut self) -> &mut [u8] {
        &mut self.working_buffer
    }

    #[cfg(test)]
    pub(crate) fn set_positions_for_test(&mut self, position: usize, length: usize) {
        self.position = position;
        self.length = length;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn change_packet_size_resize_and_noop() {
        let mut buf = PacketBuffer::with_packet_size(4096);
        assert_eq!(buf.working_buffer().len(), 8192);
        assert_eq!(buf.max_packet_size(), 4096);

        // No-op when the size is unchanged: in-flight state survives.
        buf.set_positions_for_test(100, 500);
        buf.change_packet_size(4096);
        assert_eq!(buf.position(), 100);
        assert_eq!(buf.length(), 500);
        assert_eq!(buf.working_buffer().len(), 8192);

        // Resize on change: buffer grows to size * 2 and state resets.
        buf.change_packet_size(8000);
        assert_eq!(buf.working_buffer().len(), 16000);
        assert_eq!(buf.max_packet_size(), 8000);
        assert_eq!(buf.position(), 0);
        assert_eq!(buf.length(), 0);
    }

    /// begin_refill must relocate remaining bytes before pending bytes so the
    /// two regions never overlap-corrupt each other.
    #[test]
    fn begin_refill_relocates_pending_without_corruption() {
        let mut buf = PacketBuffer::with_packet_size(4096);

        for i in 82..4088 {
            buf.working_buffer_mut()[i] = (i % 256) as u8;
        }
        buf.set_positions_for_test(82, 4088);

        let pending_start = 4088;
        let pending_len = 4096;
        for i in 0..pending_len {
            buf.working_buffer_mut()[pending_start + i] = 0xAA;
        }
        buf.set_pending_for_test(pending_len, pending_start);

        let expected_remaining: Vec<u8> = buf.working_buffer()[82..4088].to_vec();

        let (base, already) = buf.begin_refill().unwrap();
        assert_eq!(base, 4006);
        assert_eq!(already, pending_len);

        assert_eq!(&buf.working_buffer()[..4006], &expected_remaining[..]);
        assert!(
            buf.working_buffer()[4006..4006 + pending_len]
                .iter()
                .all(|&b| b == 0xAA)
        );
        assert_eq!(buf.position(), 0);
        assert_eq!(buf.length(), 4006);
        assert_eq!(buf.pending_bytes(), 0);
    }

    #[test]
    fn begin_refill_no_remaining_with_pending() {
        let mut buf = PacketBuffer::with_packet_size(4096);
        let pending_start = 4088;
        for i in 0..100 {
            buf.working_buffer_mut()[pending_start + i] = 0xBB;
        }
        buf.set_pending_for_test(100, pending_start);

        let (base, already) = buf.begin_refill().unwrap();
        assert_eq!(base, 0);
        assert_eq!(already, 100);
        assert!(buf.working_buffer()[..100].iter().all(|&b| b == 0xBB));
        assert_eq!(buf.length(), 0);
    }

    #[test]
    fn begin_refill_rejects_out_of_bounds_pending() {
        let mut buf = PacketBuffer::with_packet_size(64);
        let buffer_len = buf.working_buffer().len();
        buf.set_pending_for_test(100, buffer_len + 1000);
        assert!(buf.begin_refill().is_err());
    }

    #[test]
    fn strip_header_exposes_payload() {
        let mut buf = PacketBuffer::with_packet_size(4096);
        // 8-byte header + 92-byte payload at the current fill point (offset 0).
        for i in 8..100 {
            buf.working_buffer_mut()[i] = 0xCC;
        }
        buf.set_positions_for_test(0, 0);
        buf.strip_header(100);
        assert_eq!(buf.length(), 92);
        assert_eq!(buf.available(), 92);
        assert_eq!(buf.take_u8().unwrap(), 0xCC);
    }

    #[test]
    fn record_pending_tracks_surplus() {
        let mut buf = PacketBuffer::with_packet_size(4096);
        buf.record_pending(0, 100, 150);
        assert_eq!(buf.pending_bytes(), 50);

        buf.record_pending(0, 100, 100);
        assert_eq!(buf.pending_bytes(), 0);
    }

    #[test]
    fn reset_to_length_preserves_pending() {
        let mut buf = PacketBuffer::with_packet_size(4096);
        buf.set_positions_for_test(250, 500);
        buf.set_pending_for_test(30, 900);
        buf.reset_to_length(0);
        assert_eq!(buf.position(), 0);
        assert_eq!(buf.length(), 0);
        assert_eq!(buf.pending_bytes(), 30);
    }

    #[test]
    fn validate_packet_length_guards() {
        let mut buf = PacketBuffer::with_packet_size(512);
        // Header declaring length 4 (< 8) is rejected.
        buf.working_buffer_mut()[2] = 0x00;
        buf.working_buffer_mut()[3] = 0x04;
        assert!(buf.validate_packet_length(0).is_err());

        // Length exceeding the negotiated max packet size is rejected.
        buf.working_buffer_mut()[2] = 0xEA;
        buf.working_buffer_mut()[3] = 0x60; // 60000
        assert!(buf.validate_packet_length(0).is_err());

        // A valid length is accepted.
        buf.working_buffer_mut()[2] = 0x00;
        buf.working_buffer_mut()[3] = 0x40; // 64
        assert_eq!(buf.validate_packet_length(0).unwrap(), 64);
    }
}
