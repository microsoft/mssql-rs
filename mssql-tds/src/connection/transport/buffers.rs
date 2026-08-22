// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::fmt::Debug;
use std::ops::{Deref, DerefMut, Index, IndexMut};

use crate::core::TdsResult;
use crate::error::Error;

pub(crate) struct TdsReadBuffer {
    pub(crate) buffer_position: usize,
    pub(crate) buffer_length: usize,
    pub(crate) max_packet_size: usize,
    pub(crate) working_buffer: Vec<u8>,
    /// Bytes that have been read from the network but are beyond the current packet.
    /// This happens when a single read returns data for multiple TDS packets.
    pub(crate) pending_bytes: usize,
    /// The offset where pending bytes are located in working_buffer.
    pub(crate) pending_bytes_offset: usize,
    /// Whether the most recently framed packet carried the end-of-message flag.
    ///
    /// Once the terminating packet of a message has been framed, the server
    /// sends nothing further until the client issues a new request. Readers use
    /// this to fail a short read instead of blocking on a socket that will stay
    /// silent.
    pub(crate) end_of_message: bool,
}

impl TdsReadBuffer {
    pub(crate) fn new(packet_size: usize) -> Self {
        let packet_storage = packet_size * 2;
        Self {
            buffer_position: 0,
            buffer_length: 0,
            max_packet_size: packet_size,
            working_buffer: vec![0; packet_storage],
            pending_bytes: 0,
            pending_bytes_offset: 0,
            end_of_message: false,
        }
    }

    pub(crate) fn change_packet_size(&mut self, packet_size: u32) {
        if packet_size != self.max_packet_size as u32 {
            self.max_packet_size = packet_size as usize;
            self.working_buffer.resize(packet_size as usize * 2, 0);
            self.buffer_position = 0;
            self.buffer_length = 0;
            self.pending_bytes = 0;
            self.pending_bytes_offset = 0;
            self.end_of_message = false;
        }
    }

    pub(crate) fn do_we_have_enough_data(&self, byte_count: usize) -> bool {
        self.get_remaining_byte_count() >= byte_count
    }

    pub(crate) fn get_remaining_byte_count(&self) -> usize {
        // Saturating: `buffer_position` must never pass `buffer_length`, but a
        // plain subtraction wraps to a huge value in release builds if it ever
        // does, which would make every "do we have enough data" check pass and
        // send callers slicing past the end of the buffer.
        self.buffer_length.saturating_sub(self.buffer_position)
    }

    #[inline(always)]
    fn try_read_array<const N: usize>(&mut self) -> Option<[u8; N]> {
        if !self.do_we_have_enough_data(N) {
            return None;
        }

        let position = self.buffer_position;
        let bytes = self.working_buffer[position..position + N]
            .try_into()
            .expect("slice length is fixed by N");
        self.consume_bytes(N).ok()?;
        Some(bytes)
    }

    #[inline(always)]
    pub(crate) fn try_read_byte(&mut self) -> Option<u8> {
        self.try_read_array().map(|[value]| value)
    }

    #[inline(always)]
    pub(crate) fn try_read_int16(&mut self) -> Option<i16> {
        self.try_read_array().map(i16::from_le_bytes)
    }

    #[inline(always)]
    pub(crate) fn try_read_uint16(&mut self) -> Option<u16> {
        self.try_read_array().map(u16::from_le_bytes)
    }

    #[inline(always)]
    pub(crate) fn try_read_uint24(&mut self) -> Option<u32> {
        let [b0, b1, b2] = self.try_read_array()?;
        Some(u32::from_le_bytes([b0, b1, b2, 0]))
    }

    #[inline(always)]
    pub(crate) fn try_read_int32(&mut self) -> Option<i32> {
        self.try_read_array().map(i32::from_le_bytes)
    }

    #[inline(always)]
    pub(crate) fn try_read_uint32(&mut self) -> Option<u32> {
        self.try_read_array().map(u32::from_le_bytes)
    }

    #[inline(always)]
    pub(crate) fn try_read_uint40(&mut self) -> Option<u64> {
        let [b0, b1, b2, b3, b4] = self.try_read_array()?;
        Some(u64::from_le_bytes([b0, b1, b2, b3, b4, 0, 0, 0]))
    }

    #[inline(always)]
    pub(crate) fn try_read_int64(&mut self) -> Option<i64> {
        self.try_read_array().map(i64::from_le_bytes)
    }

    #[inline(always)]
    pub(crate) fn try_read_float32(&mut self) -> Option<f32> {
        self.try_read_array().map(f32::from_le_bytes)
    }

    #[inline(always)]
    pub(crate) fn try_read_float64(&mut self) -> Option<f64> {
        self.try_read_array().map(f64::from_le_bytes)
    }

    pub(crate) fn consume_bytes(&mut self, byte_count: usize) -> TdsResult<()> {
        let remaining = self.get_remaining_byte_count();
        if byte_count > remaining {
            return Err(Error::ProtocolError(format!(
                "Cannot consume {byte_count} byte(s) from the packet buffer: only {remaining} remain"
            )));
        }

        self.buffer_position += byte_count;
        if self.buffer_length == self.buffer_position {
            self.buffer_length = 0;
            self.buffer_position = 0;
        }
        Ok(())
    }

    /// Sets the position to 0 and the length to the specified length.
    ///
    /// Also clears the end-of-message flag: the buffer no longer holds a framed
    /// packet, so the previous message's terminator says nothing about what
    /// comes next.
    pub(crate) fn reset_to_length(&mut self, length: usize) {
        self.buffer_position = 0;
        self.buffer_length = length;
        self.end_of_message = false;
    }

    pub(crate) fn shift_data_to_front(&mut self) {
        let remaining = self.get_remaining_byte_count();

        // Move the remaining data to front FIRST, before touching pending bytes.
        // This prevents the pending copy from clobbering the tail of the remaining
        // data when the pending region overlaps with [buffer_position..buffer_length].
        self.working_buffer
            .copy_within(self.buffer_position..self.buffer_length, 0);
        self.buffer_position = 0;
        self.buffer_length = remaining;

        // Now move pending bytes right after the (already relocated) remaining data.
        if self.pending_bytes > 0 {
            let pending_src_start = self.pending_bytes_offset;
            let pending_src_end = self.pending_bytes_offset + self.pending_bytes;
            let pending_dest = remaining;
            self.working_buffer
                .copy_within(pending_src_start..pending_src_end, pending_dest);
            self.pending_bytes_offset = remaining;
        }
    }

    pub(crate) fn remove_header_from_packet(&mut self, new_packet_size: usize) {
        self.working_buffer.copy_within(
            self.buffer_length + 8..self.buffer_length + new_packet_size,
            self.buffer_length,
        );
        self.buffer_length += new_packet_size - 8;
    }

    pub(crate) fn get_slice(&self) -> &[u8] {
        &self.working_buffer[self.buffer_position..]
    }
}

impl Debug for TdsReadBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TdsReadBuffer")
            .field("buffer_position", &self.buffer_position)
            .field("buffer_length", &self.buffer_length)
            .field("max_packet_size", &self.max_packet_size)
            .finish()
    }
}

impl Index<usize> for TdsReadBuffer {
    type Output = u8;

    fn index(&self, index: usize) -> &Self::Output {
        &self.working_buffer[index]
    }
}

impl IndexMut<usize> for TdsReadBuffer {
    fn index_mut(&mut self, index: usize) -> &mut Self::Output {
        &mut self.working_buffer[index]
    }
}

impl Deref for TdsReadBuffer {
    type Target = Vec<u8>;
    fn deref(&self) -> &Vec<u8> {
        &self.working_buffer
    }
}

impl DerefMut for TdsReadBuffer {
    fn deref_mut(&mut self) -> &mut Vec<u8> {
        &mut self.working_buffer
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that demonstrates the buffer overflow bug when reset_reader() doesn't call
    /// change_packet_size() after packet size negotiation.
    ///
    /// Scenario:
    /// 1. Pre-login: packet_size = 4096, buffer = 8192 bytes (4096 × 2)
    /// 2. Login completes: server negotiates packet_size = 8000
    /// 3. BUG: If reset_reader() only calls reset_to_length(0) without change_packet_size(),
    ///    the buffer remains at 8192 bytes
    /// 4. Server sends 8000-byte packet → code tries to read beyond buffer → panic
    ///
    /// FIX: reset_reader() must call change_packet_size() to resize buffer to 16000 bytes
    #[test]
    fn test_buffer_resize_after_packet_size_change() {
        // Initial state: pre-login packet size of 4096
        let initial_packet_size: usize = 4096;
        let mut buffer = TdsReadBuffer::new(initial_packet_size);

        // Verify initial buffer size: 4096 * 2 = 8192
        assert_eq!(buffer.working_buffer.len(), 8192);
        assert_eq!(buffer.max_packet_size, 4096);

        // Simulate packet size negotiation to 8000 (like after login)
        let negotiated_packet_size: u32 = 8000;

        // BUG SIMULATION: Only reset_to_length without change_packet_size
        // This is what the buggy TdsTransport::reset_reader() was doing
        buffer.reset_to_length(0);

        // Buffer is still 8192 - NOT enough for 8000 * 2 = 16000
        assert_eq!(buffer.working_buffer.len(), 8192);
        assert_eq!(buffer.max_packet_size, 4096); // Still old value!

        // This would cause a panic when trying to read a packet larger than 8192/2 = 4096
        // because read_tds_packet reads into base_offset + max_packet_size slice

        // FIX: Call change_packet_size BEFORE reset_to_length
        buffer.change_packet_size(negotiated_packet_size);

        // Now buffer is properly sized: 8000 * 2 = 16000
        assert_eq!(buffer.working_buffer.len(), 16000);
        assert_eq!(buffer.max_packet_size, 8000);

        // Safe to read 8000-byte packets now
        assert!(buffer.working_buffer.len() >= negotiated_packet_size as usize * 2);
    }

    /// Test that change_packet_size is idempotent when called with the same size
    #[test]
    fn test_change_packet_size_same_size_is_noop() {
        let packet_size: usize = 4096;
        let mut buffer = TdsReadBuffer::new(packet_size);

        // Set some state
        buffer.buffer_position = 100;
        buffer.buffer_length = 500;

        // Call with same size - should be no-op (preserves state)
        buffer.change_packet_size(packet_size as u32);

        // State should be preserved since size didn't change
        assert_eq!(buffer.buffer_position, 100);
        assert_eq!(buffer.buffer_length, 500);
        assert_eq!(buffer.working_buffer.len(), 8192);
    }

    /// Test that change_packet_size resets buffer state when size changes
    #[test]
    fn test_change_packet_size_resets_state_on_size_change() {
        let initial_size: usize = 4096;
        let mut buffer = TdsReadBuffer::new(initial_size);

        // Set some state
        buffer.buffer_position = 100;
        buffer.buffer_length = 500;

        // Change to different size - should reset state
        buffer.change_packet_size(8000);

        // State should be reset
        assert_eq!(buffer.buffer_position, 0);
        assert_eq!(buffer.buffer_length, 0);
        assert_eq!(buffer.working_buffer.len(), 16000);
        assert_eq!(buffer.max_packet_size, 8000);
    }

    /// Reproduces data corruption when shift_data_to_front moves pending bytes
    /// before relocating remaining data, causing an overlap that clobbers the
    /// tail of the remaining region.
    ///
    /// Layout before shift (packet_size=4096, buffer=8192):
    ///   [consumed 82B | remaining 4006B | pending 4096B at offset 4088]
    ///
    /// Bug: copying pending to offset 4006 overwrites remaining[4006..4088].
    #[test]
    fn test_shift_data_to_front_with_pending_bytes_no_corruption() {
        let mut buf = TdsReadBuffer::new(4096);

        // Fill the "remaining" region [82..4088] with recognizable data.
        for i in 82..4088 {
            buf.working_buffer[i] = (i % 256) as u8;
        }
        buf.buffer_position = 82;
        buf.buffer_length = 4088;

        // Simulate pending bytes from a second TDS packet right after.
        let pending_start = 4088;
        let pending_len = 4096;
        for i in 0..pending_len {
            buf.working_buffer[pending_start + i] = 0xAA;
        }
        buf.pending_bytes = pending_len;
        buf.pending_bytes_offset = pending_start;

        // Snapshot the remaining data before shifting.
        let expected_remaining: Vec<u8> = buf.working_buffer[82..4088].to_vec();

        buf.shift_data_to_front();

        // Remaining data must be intact at [0..4006].
        assert_eq!(
            &buf.working_buffer[..4006],
            &expected_remaining[..],
            "remaining data corrupted after shift_data_to_front"
        );

        // Pending data must follow at [4006..4006+4096].
        assert!(
            buf.working_buffer[4006..4006 + pending_len]
                .iter()
                .all(|&b| b == 0xAA),
            "pending data not correctly placed after remaining"
        );

        assert_eq!(buf.buffer_position, 0);
        assert_eq!(buf.buffer_length, 4006);
        assert_eq!(buf.pending_bytes_offset, 4006);
        assert_eq!(buf.pending_bytes, pending_len);
    }

    /// Simulates a partially-consumed buffer with pending bytes from a second
    /// TCP read. The consumed (position) region is non-zero, remaining data
    /// sits in the middle, and pending bytes follow at the end.
    #[test]
    fn test_shift_data_to_front_consumed_remaining_and_pending() {
        let mut buf = TdsReadBuffer::new(4096);

        // [0..500] consumed, [500..2000] remaining, [4088..4088+200] pending
        for i in 500..2000 {
            buf.working_buffer[i] = (i % 256) as u8;
        }
        buf.buffer_position = 500;
        buf.buffer_length = 2000;

        let pending_start = 4088;
        let pending_len = 200;
        for i in 0..pending_len {
            buf.working_buffer[pending_start + i] = 0xDD;
        }
        buf.pending_bytes = pending_len;
        buf.pending_bytes_offset = pending_start;

        let expected_remaining: Vec<u8> = buf.working_buffer[500..2000].to_vec();

        buf.shift_data_to_front();

        let remaining = 1500;
        assert_eq!(buf.buffer_position, 0);
        assert_eq!(buf.buffer_length, remaining);
        assert_eq!(
            &buf.working_buffer[..remaining],
            &expected_remaining[..],
            "remaining data corrupted"
        );
        assert_eq!(buf.pending_bytes_offset, remaining);
        assert_eq!(buf.pending_bytes, pending_len);
        assert!(
            buf.working_buffer[remaining..remaining + pending_len]
                .iter()
                .all(|&b| b == 0xDD),
            "pending data corrupted or misplaced"
        );
    }

    #[test]
    fn test_shift_data_to_front_no_pending_bytes() {
        let mut buf = TdsReadBuffer::new(4096);
        for i in 100..500 {
            buf.working_buffer[i] = (i % 256) as u8;
        }
        buf.buffer_position = 100;
        buf.buffer_length = 500;

        let expected: Vec<u8> = buf.working_buffer[100..500].to_vec();
        buf.shift_data_to_front();

        assert_eq!(&buf.working_buffer[..400], &expected[..]);
        assert_eq!(buf.buffer_position, 0);
        assert_eq!(buf.buffer_length, 400);
    }

    #[test]
    fn test_shift_data_to_front_already_at_zero() {
        let mut buf = TdsReadBuffer::new(4096);
        for i in 0..200 {
            buf.working_buffer[i] = (i % 256) as u8;
        }
        buf.buffer_position = 0;
        buf.buffer_length = 200;

        let expected: Vec<u8> = buf.working_buffer[..200].to_vec();
        buf.shift_data_to_front();

        assert_eq!(&buf.working_buffer[..200], &expected[..]);
        assert_eq!(buf.buffer_position, 0);
        assert_eq!(buf.buffer_length, 200);
    }

    #[test]
    fn test_shift_data_to_front_no_remaining_with_pending() {
        let mut buf = TdsReadBuffer::new(4096);
        buf.buffer_position = 0;
        buf.buffer_length = 0;

        let pending_start = 4088;
        for i in 0..100 {
            buf.working_buffer[pending_start + i] = 0xBB;
        }
        buf.pending_bytes = 100;
        buf.pending_bytes_offset = pending_start;

        buf.shift_data_to_front();

        assert_eq!(buf.buffer_position, 0);
        assert_eq!(buf.buffer_length, 0);
        assert_eq!(buf.pending_bytes_offset, 0);
        assert!(buf.working_buffer[..100].iter().all(|&b| b == 0xBB));
    }

    #[test]
    fn test_consume_bytes_partial() {
        let mut buf = TdsReadBuffer::new(4096);
        buf.buffer_length = 500;
        buf.buffer_position = 0;

        buf.consume_bytes(200)
            .expect("200 of 500 bytes must consume");

        assert_eq!(buf.buffer_position, 200);
        assert_eq!(buf.buffer_length, 500);
    }

    #[test]
    fn test_consume_bytes_exact_resets() {
        let mut buf = TdsReadBuffer::new(4096);
        buf.buffer_length = 500;
        buf.buffer_position = 100;

        buf.consume_bytes(400)
            .expect("the exact remaining count must consume");

        assert_eq!(buf.buffer_position, 0);
        assert_eq!(buf.buffer_length, 0);
    }

    #[test]
    fn test_consume_bytes_over_errors() {
        let mut buf = TdsReadBuffer::new(4096);
        buf.buffer_length = 500;
        buf.buffer_position = 100;

        // Consuming past the end used to `panic!`. In a driver loaded into a
        // host process (ODBC, Node, Python) that turns a recoverable protocol
        // fault into a crash, so it reports an error instead.
        let error = buf
            .consume_bytes(401)
            .expect_err("consuming past the end must be an error");

        assert!(
            matches!(error, Error::ProtocolError(ref message) if message.contains("only 400 remain")),
            "expected a protocol error naming the shortfall, got {error:?}"
        );
        assert_eq!(buf.buffer_position, 100, "a rejected consume must not move");
        assert_eq!(buf.buffer_length, 500);
    }

    /// `buffer_position` must never pass `buffer_length`, but if it ever did a
    /// plain subtraction would wrap to a huge value in release builds and make
    /// every capacity check pass.
    #[test]
    fn remaining_byte_count_saturates_when_position_leads_length() {
        let mut buf = TdsReadBuffer::new(4096);
        buf.buffer_length = 100;
        buf.buffer_position = 250;

        assert_eq!(buf.get_remaining_byte_count(), 0);
        assert!(!buf.do_we_have_enough_data(1));
        assert!(buf.consume_bytes(1).is_err());
    }

    #[test]
    fn test_do_we_have_enough_data() {
        let mut buf = TdsReadBuffer::new(4096);
        buf.buffer_length = 500;
        buf.buffer_position = 100;

        assert!(buf.do_we_have_enough_data(400));
        assert!(buf.do_we_have_enough_data(1));
        assert!(!buf.do_we_have_enough_data(401));
    }

    #[test]
    fn test_fixed_scalar_probes_read_complete_values() {
        let expected_byte = 0xAB;
        let expected_int16 = -0x1234i16;
        let expected_uint16 = 0x1234u16;
        let expected_uint24 = 0x00A1_B2C3u32;
        let expected_int32 = -0x0123_4567i32;
        let expected_uint32 = 0x89AB_CDEFu32;
        let expected_uint40 = 0xAB_CDEF_0123u64;
        let expected_int64 = -0x0102_0304_0506_0708i64;
        let expected_float32 = 1.5f32;
        let expected_float64 = -2.25f64;

        let mut bytes = Vec::new();
        bytes.push(expected_byte);
        bytes.extend_from_slice(&expected_int16.to_le_bytes());
        bytes.extend_from_slice(&expected_uint16.to_le_bytes());
        bytes.extend_from_slice(&expected_uint24.to_le_bytes()[..3]);
        bytes.extend_from_slice(&expected_int32.to_le_bytes());
        bytes.extend_from_slice(&expected_uint32.to_le_bytes());
        bytes.extend_from_slice(&expected_uint40.to_le_bytes()[..5]);
        bytes.extend_from_slice(&expected_int64.to_le_bytes());
        bytes.extend_from_slice(&expected_float32.to_le_bytes());
        bytes.extend_from_slice(&expected_float64.to_le_bytes());

        let mut buf = TdsReadBuffer::new(4096);
        buf.working_buffer[..bytes.len()].copy_from_slice(&bytes);
        buf.reset_to_length(bytes.len());

        assert_eq!(buf.try_read_byte(), Some(expected_byte));
        assert_eq!(buf.try_read_int16(), Some(expected_int16));
        assert_eq!(buf.try_read_uint16(), Some(expected_uint16));
        assert_eq!(buf.try_read_uint24(), Some(expected_uint24));
        assert_eq!(buf.try_read_int32(), Some(expected_int32));
        assert_eq!(buf.try_read_uint32(), Some(expected_uint32));
        assert_eq!(buf.try_read_uint40(), Some(expected_uint40));
        assert_eq!(buf.try_read_int64(), Some(expected_int64));
        assert_eq!(buf.try_read_float32(), Some(expected_float32));
        assert_eq!(buf.try_read_float64(), Some(expected_float64));
        assert_eq!(buf.get_remaining_byte_count(), 0);
    }

    #[test]
    fn test_fixed_scalar_probe_misses_do_not_consume() {
        let mut buf = TdsReadBuffer::new(4096);

        macro_rules! assert_miss_does_not_consume {
            ($partial_len:expr, $method:ident) => {{
                buf.working_buffer[..$partial_len].fill(0xA5);
                buf.reset_to_length($partial_len);
                assert_eq!(buf.$method(), None);
                assert_eq!(buf.buffer_position, 0);
                assert_eq!(buf.get_remaining_byte_count(), $partial_len);
            }};
        }

        assert_miss_does_not_consume!(0, try_read_byte);
        assert_miss_does_not_consume!(1, try_read_int16);
        assert_miss_does_not_consume!(1, try_read_uint16);
        assert_miss_does_not_consume!(2, try_read_uint24);
        assert_miss_does_not_consume!(3, try_read_int32);
        assert_miss_does_not_consume!(3, try_read_uint32);
        assert_miss_does_not_consume!(4, try_read_uint40);
        assert_miss_does_not_consume!(7, try_read_int64);
        assert_miss_does_not_consume!(3, try_read_float32);
        assert_miss_does_not_consume!(7, try_read_float64);
    }

    #[test]
    fn test_get_remaining_byte_count() {
        let mut buf = TdsReadBuffer::new(4096);
        buf.buffer_length = 500;
        buf.buffer_position = 100;

        assert_eq!(buf.get_remaining_byte_count(), 400);

        buf.consume_bytes(200)
            .expect("200 of 400 bytes must consume");
        assert_eq!(buf.get_remaining_byte_count(), 200);
    }

    #[test]
    fn test_reset_to_length() {
        let mut buf = TdsReadBuffer::new(4096);
        buf.buffer_position = 250;
        buf.buffer_length = 500;

        buf.reset_to_length(1000);

        assert_eq!(buf.buffer_position, 0);
        assert_eq!(buf.buffer_length, 1000);
    }

    #[test]
    fn test_remove_header_from_packet() {
        let mut buf = TdsReadBuffer::new(4096);
        buf.buffer_length = 100;

        // Place a fake packet at offset 100: 8-byte header + 92 bytes payload = 100 bytes.
        let header = [0x04, 0x00, 0x00, 0x64, 0x00, 0x00, 0x01, 0x00];
        buf.working_buffer[100..108].copy_from_slice(&header);
        for i in 108..200 {
            buf.working_buffer[i] = 0xCC;
        }

        buf.remove_header_from_packet(100);

        assert_eq!(buf.buffer_length, 192);
        assert!(buf.working_buffer[100..192].iter().all(|&b| b == 0xCC));
    }

    #[test]
    fn test_get_slice_returns_from_position() {
        let mut buf = TdsReadBuffer::new(4096);
        buf.working_buffer[50] = 0xDE;
        buf.working_buffer[51] = 0xAD;
        buf.buffer_position = 50;

        let slice = buf.get_slice();
        assert_eq!(slice[0], 0xDE);
        assert_eq!(slice[1], 0xAD);
    }
}
