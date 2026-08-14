// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use std::fmt::Debug;
use std::ops::{Deref, DerefMut, Index, IndexMut};

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
        }
    }

    pub(crate) fn do_we_have_enough_data(&self, byte_count: usize) -> bool {
        let remaining_bytes = self.buffer_length - self.buffer_position;
        remaining_bytes >= byte_count
    }

    pub(crate) fn get_remaining_byte_count(&self) -> usize {
        self.buffer_length - self.buffer_position
    }

    #[inline(always)]
    pub(crate) fn try_read_byte(&mut self) -> Option<u8> {
        if !self.do_we_have_enough_data(1) {
            return None;
        }

        let value = self.working_buffer[self.buffer_position];
        self.consume_bytes(1);
        Some(value)
    }

    #[inline(always)]
    pub(crate) fn try_read_uint16(&mut self) -> Option<u16> {
        if !self.do_we_have_enough_data(2) {
            return None;
        }

        let position = self.buffer_position;
        let value = u16::from_le_bytes([
            self.working_buffer[position],
            self.working_buffer[position + 1],
        ]);
        self.consume_bytes(2);
        Some(value)
    }

    #[inline(always)]
    pub(crate) fn try_read_int32(&mut self) -> Option<i32> {
        if !self.do_we_have_enough_data(4) {
            return None;
        }

        let position = self.buffer_position;
        let value = i32::from_le_bytes([
            self.working_buffer[position],
            self.working_buffer[position + 1],
            self.working_buffer[position + 2],
            self.working_buffer[position + 3],
        ]);
        self.consume_bytes(4);
        Some(value)
    }

    pub(crate) fn consume_bytes(&mut self, byte_count: usize) {
        if byte_count > (self.buffer_length - self.buffer_position) {
            panic!("Not enough data to consume");
        }

        self.buffer_position += byte_count;
        if self.buffer_length == self.buffer_position {
            self.buffer_length = 0;
            self.buffer_position = 0;
        }
    }

    /// Sets the position to 0 and the length to the specified length.
    pub(crate) fn reset_to_length(&mut self, length: usize) {
        self.buffer_position = 0;
        self.buffer_length = length;
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

        buf.consume_bytes(200);

        assert_eq!(buf.buffer_position, 200);
        assert_eq!(buf.buffer_length, 500);
    }

    #[test]
    fn test_consume_bytes_exact_resets() {
        let mut buf = TdsReadBuffer::new(4096);
        buf.buffer_length = 500;
        buf.buffer_position = 100;

        buf.consume_bytes(400);

        assert_eq!(buf.buffer_position, 0);
        assert_eq!(buf.buffer_length, 0);
    }

    #[test]
    #[should_panic(expected = "Not enough data to consume")]
    fn test_consume_bytes_over_panics() {
        let mut buf = TdsReadBuffer::new(4096);
        buf.buffer_length = 500;
        buf.buffer_position = 100;

        buf.consume_bytes(401);
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
    fn test_small_scalar_probes_read_complete_values() {
        let mut buf = TdsReadBuffer::new(4096);
        buf.working_buffer[..7].copy_from_slice(&[0xAB, 0x34, 0x12, 0x78, 0x56, 0x34, 0x12]);
        buf.reset_to_length(7);

        assert_eq!(buf.try_read_byte(), Some(0xAB));
        assert_eq!(buf.try_read_uint16(), Some(0x1234));
        assert_eq!(buf.try_read_int32(), Some(0x1234_5678));
        assert_eq!(buf.get_remaining_byte_count(), 0);
    }

    #[test]
    fn test_small_scalar_probe_misses_do_not_consume() {
        let mut buf = TdsReadBuffer::new(4096);

        assert_eq!(buf.try_read_byte(), None);
        assert_eq!(buf.buffer_position, 0);

        buf.working_buffer[0] = 0x34;
        buf.reset_to_length(1);
        assert_eq!(buf.try_read_uint16(), None);
        assert_eq!(buf.buffer_position, 0);
        assert_eq!(buf.get_remaining_byte_count(), 1);

        buf.working_buffer[..3].copy_from_slice(&[0x78, 0x56, 0x34]);
        buf.reset_to_length(3);
        assert_eq!(buf.try_read_int32(), None);
        assert_eq!(buf.buffer_position, 0);
        assert_eq!(buf.get_remaining_byte_count(), 3);
    }

    #[test]
    fn test_get_remaining_byte_count() {
        let mut buf = TdsReadBuffer::new(4096);
        buf.buffer_length = 500;
        buf.buffer_position = 100;

        assert_eq!(buf.get_remaining_byte_count(), 400);

        buf.consume_bytes(200);
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
