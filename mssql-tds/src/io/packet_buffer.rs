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

/// Sans-I/O signal that a synchronous read needs more bytes than are currently
/// buffered.
///
/// This is the one thing the sync core cannot resolve on its own. Returned by
/// [`PacketBuffer::ensure`] without consuming anything, it tells the async shell
/// exactly how many more bytes to pull off the wire before re-driving the read.
/// Because the guard is a pure check, the paired `take_*` accessors stay atomic:
/// they only advance the read position when the full width is present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NeedBytes {
    /// Additional bytes required beyond what is currently available.
    pub(crate) shortfall: usize,
}

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

    /// Sync-core guard for a fixed-width read: succeeds when `byte_count` bytes
    /// can be taken without a refill, otherwise reports the [`NeedBytes`]
    /// shortfall without consuming anything.
    ///
    /// This is the sole suspension point of the inverted read path. The async
    /// shell drives it in a loop — refilling on `NeedBytes` and retrying — while
    /// the paired atomic `take_*` accessors guarantee no partial advance, so a
    /// read is always safe to re-drive after a refill.
    pub(crate) fn ensure(&self, byte_count: usize) -> Result<(), NeedBytes> {
        let available = self.available();
        if available >= byte_count {
            Ok(())
        } else {
            Err(NeedBytes {
                shortfall: byte_count - available,
            })
        }
    }

    /// Readable bytes as a slice (from the current position to the filled end).
    fn peek(&self) -> &[u8] {
        &self.working_buffer[self.position..self.length]
    }

    /// Non-consuming view of the first `n` readable bytes, or `None` when fewer
    /// than `n` are buffered.
    ///
    /// Unlike [`take`](Self::take) this never advances the read position, so the
    /// column-atomic decode path can inspect a length prefix and then re-drive
    /// from the column start after a refill without having consumed anything.
    pub(crate) fn peek_bytes(&self, n: usize) -> Option<&[u8]> {
        if self.has(n) {
            Some(&self.working_buffer[self.position..self.position + n])
        } else {
            None
        }
    }

    /// Builds a transient, refill-free buffer whose entire readable payload is
    /// `bytes`.
    ///
    /// Used by the async column driver to stage a fully-assembled non-PLP cell
    /// so the single synchronous `decode_column_body` can run over it, without a
    /// socket or packet framing. There is no header and no refill source, so the
    /// paired `take_*` accessors serve straight from `bytes`.
    pub(crate) fn from_bytes(bytes: &[u8]) -> Self {
        let len = bytes.len();
        PacketBuffer {
            working_buffer: bytes.to_vec(),
            position: 0,
            length: len,
            max_packet_size: len.max(1),
            pending_bytes: 0,
            pending_bytes_offset: 0,
        }
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

    /// Atomically consumes and returns the next `n` readable bytes.
    ///
    /// The owned counterpart to the scalar `take_*` accessors: callers `ensure`
    /// residency first, so a shortfall here is a logic error, not a request for
    /// more data. The take is all-or-nothing — nothing is consumed on error.
    pub(crate) fn take_bytes(&mut self, n: usize) -> TdsResult<Vec<u8>> {
        Ok(self.take(n)?.to_vec())
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

    /// ensure() is a pure guard: it never consumes, reports the exact shortfall
    /// on an empty/short buffer, and succeeds once enough bytes are present.
    #[test]
    fn ensure_reports_shortfall_without_consuming() {
        let mut buf = PacketBuffer::with_packet_size(4096);

        // Empty buffer: a 4-byte read is short by the full width.
        assert_eq!(buf.ensure(4), Err(NeedBytes { shortfall: 4 }));
        assert_eq!(buf.available(), 0);

        // Expose exactly 2 payload bytes.
        buf.working_buffer_mut()[8] = 0xDE;
        buf.working_buffer_mut()[9] = 0xAD;
        buf.set_positions_for_test(0, 0);
        buf.strip_header(10);
        assert_eq!(buf.available(), 2);

        // Short read reports the remaining shortfall and consumes nothing.
        assert_eq!(buf.ensure(4), Err(NeedBytes { shortfall: 2 }));
        assert_eq!(buf.available(), 2);

        // Exact-boundary read is allowed and still non-consuming.
        assert_eq!(buf.ensure(2), Ok(()));
        assert_eq!(buf.available(), 2);

        // The atomic take advances only after a satisfied guard.
        assert_eq!(buf.take_u16_le().unwrap(), 0xADDE);
        assert_eq!(buf.available(), 0);
        assert_eq!(buf.ensure(1), Err(NeedBytes { shortfall: 1 }));
    }

    /// Stages `bytes` as fully-buffered readable payload at position 0, without
    /// staging a header or socket read — the fast path for exercising the
    /// positional accessors directly.
    fn staged(bytes: &[u8]) -> PacketBuffer {
        let mut buf = PacketBuffer::with_packet_size(4096);
        buf.working_buffer_mut()[..bytes.len()].copy_from_slice(bytes);
        buf.set_positions_for_test(0, bytes.len());
        buf
    }

    /// Each fixed-width accessor decodes the right bytes and advances the read
    /// position by exactly its width. A trailing sentinel byte keeps the buffer
    /// live so the advance is observable (a full drain resets the cursor — see
    /// [`take_exact_drain_resets_cursor`]).
    #[test]
    fn take_scalars_roundtrip_and_advance() {
        let mut buf = staged(&[0x7F, 0xFF]);
        assert_eq!(buf.take_u8().unwrap(), 0x7F);
        assert_eq!(buf.position(), 1);
        assert_eq!(buf.available(), 1);

        let mut buf = staged(&[0x34, 0x12, 0x00]);
        assert_eq!(buf.take_u16_le().unwrap(), 0x1234);
        assert_eq!(buf.position(), 2);

        let mut buf = staged(&[0x12, 0x34, 0x00]);
        assert_eq!(buf.take_i16_be().unwrap(), 0x1234);
        assert_eq!(buf.position(), 2);

        let mut buf = staged(&[0x56, 0x34, 0x12, 0x00]);
        assert_eq!(buf.take_u24_le().unwrap(), 0x0012_3456);
        assert_eq!(buf.position(), 3);

        let mut buf = staged(&[0x78, 0x56, 0x34, 0x12, 0x00]);
        assert_eq!(buf.take_u32_le().unwrap(), 0x1234_5678);
        assert_eq!(buf.position(), 4);

        let mut buf = staged(&[0x12, 0x34, 0x56, 0x78, 0x00]);
        assert_eq!(buf.take_i32_be().unwrap(), 0x1234_5678);
        assert_eq!(buf.position(), 4);

        let mut buf = staged(&[1, 0, 0, 0, 0, 0, 0, 0, 0xFF]);
        assert_eq!(buf.take_u64_le().unwrap(), 1);
        assert_eq!(buf.position(), 8);
    }

    /// The remaining signed/float/40-bit accessors round-trip through their
    /// little-endian decoders.
    #[test]
    fn take_scalar_breadth_decodes_all_widths() {
        let mut buf = staged(&(-12345i16).to_le_bytes());
        assert_eq!(buf.take_i16_le().unwrap(), -12345);

        let mut buf = staged(&(-123456789i32).to_le_bytes());
        assert_eq!(buf.take_i32_le().unwrap(), -123456789);

        let mut buf = staged(&(-1234567890123i64).to_le_bytes());
        assert_eq!(buf.take_i64_le().unwrap(), -1234567890123);

        let mut buf = staged(&std::f32::consts::PI.to_le_bytes());
        assert_eq!(buf.take_f32_le().unwrap(), std::f32::consts::PI);

        let mut buf = staged(&std::f64::consts::E.to_le_bytes());
        assert_eq!(buf.take_f64_le().unwrap(), std::f64::consts::E);

        // 40-bit unsigned: the low five bytes, little-endian.
        let mut buf = staged(&[0x05, 0x04, 0x03, 0x02, 0x01]);
        assert_eq!(buf.take_uint40_le().unwrap(), 0x01_0203_0405);
    }

    /// A read that consumes the last buffered byte collapses the cursor back to
    /// the front, leaving a clean empty buffer ready for the next refill.
    #[test]
    fn take_exact_drain_resets_cursor() {
        let mut buf = staged(&[0x34, 0x12]);
        assert_eq!(buf.take_u16_le().unwrap(), 0x1234);
        assert_eq!(buf.position(), 0);
        assert_eq!(buf.length(), 0);
        assert_eq!(buf.available(), 0);
    }

    /// Atomicity: a fixed-width read wider than the buffer holds must not
    /// partially advance. The bytes stay intact so the read is safe to re-drive
    /// after a refill.
    #[test]
    fn take_on_short_buffer_is_noop() {
        let mut buf = staged(&[0xAA, 0xBB]);
        assert!(buf.take_u32_le().is_err());
        assert_eq!(buf.position(), 0);
        assert_eq!(buf.available(), 2);
        assert_eq!(buf.take_u16_le().unwrap(), 0xBBAA);
    }

    /// `skip_available` and `copy_out` operate from the current position and are
    /// bounded by what is buffered — never reading or discarding past the end.
    #[test]
    fn skip_and_copy_out_advance_from_current_position() {
        let mut buf = staged(&[0x01, 0x02, 0x03, 0x04, 0x05]);

        assert_eq!(buf.skip_available(2), 2);
        assert_eq!(buf.position(), 2);
        assert_eq!(buf.available(), 3);

        let mut dst = [0u8; 2];
        assert_eq!(buf.copy_out(&mut dst), 2);
        assert_eq!(dst, [0x03, 0x04]);
        assert_eq!(buf.available(), 1);

        // Requesting more than remains copies only the tail — no over-read.
        let mut big = [0u8; 8];
        assert_eq!(buf.copy_out(&mut big), 1);
        assert_eq!(big[0], 0x05);
        assert_eq!(buf.position(), 0);
        assert_eq!(buf.length(), 0);
    }

    /// `skip_available` saturates at what is buffered instead of advancing the
    /// cursor past the filled end.
    #[test]
    fn skip_available_saturates_at_available() {
        let mut buf = staged(&[0xAA, 0xBB]);
        assert_eq!(buf.skip_available(10), 2);
        assert_eq!(buf.available(), 0);
        assert_eq!(buf.position(), 0);
    }

    /// Over-consume bounds guard: a `take` wider than what is buffered is
    /// rejected outright and leaves the cursor untouched — no silent under-read,
    /// no corruption. This is the backstop for the L3 loop-termination
    /// invariant: a failed read reports a shortfall to re-drive rather than
    /// returning garbage or advancing past the end.
    #[test]
    fn take_beyond_available_errors_without_corrupting_cursor() {
        let mut buf = staged(&[0xAA, 0xBB, 0xCC]);

        let err = buf.take_u32_le().unwrap_err();
        assert!(
            err.to_string().contains("Buffer underflow"),
            "expected underflow guard, got: {err}"
        );
        assert_eq!(buf.position(), 0);
        assert_eq!(buf.length(), 3);
        assert_eq!(buf.available(), 3);

        // ensure agrees a refill is needed rather than signalling readiness.
        assert_eq!(buf.ensure(4), Err(NeedBytes { shortfall: 1 }));

        // After a satisfied guard the same bytes read back intact.
        assert_eq!(buf.take_u8().unwrap(), 0xAA);
    }

    /// `peek_bytes` returns a non-consuming prefix view and yields `None` once
    /// the request exceeds what is buffered — the length-prefix inspection the
    /// column-atomic decode path relies on to re-drive without consuming.
    #[test]
    fn peek_bytes_is_non_consuming_and_bounded() {
        let mut buf = staged(&[0x04, 0x00, 0xAA, 0xBB]);

        // Peeking the 2-byte USHORT length prefix does not advance the cursor.
        assert_eq!(buf.peek_bytes(2), Some([0x04, 0x00].as_slice()));
        assert_eq!(buf.position(), 0);
        assert_eq!(buf.available(), 4);

        // Repeated peeks are idempotent.
        assert_eq!(buf.peek_bytes(1), Some([0x04].as_slice()));
        assert_eq!(buf.peek_bytes(4), Some([0x04, 0x00, 0xAA, 0xBB].as_slice()));

        // Beyond what is buffered => None (a refill request, not a peek).
        assert_eq!(buf.peek_bytes(5), None);

        // The bytes are still fully readable afterwards.
        assert_eq!(buf.take_u16_le().unwrap(), 0x0004);
        assert_eq!(buf.position(), 2);
    }
}
