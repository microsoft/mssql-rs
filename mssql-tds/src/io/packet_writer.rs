// Copyright (c) Microsoft Corporation.
// Licensed under the MIT License.

use super::reader_writer::NetworkWriter;
use crate::core::{CancelHandle, TdsResult};
use crate::error::Error::TimeoutError;
use crate::error::TimeoutErrorType;
use crate::message::messages::{PacketStatusFlags, PacketType};
use async_trait::async_trait;
use byteorder::{BigEndian, WriteBytesExt};
use std::io::Cursor;
use std::time::Instant;
use tracing::event;

/// Optimized batch write operations with manual overflow control.
/// Use this for performance-critical code paths where you can batch multiple writes.
pub(crate) trait TdsPacketWriterUnchecked {
    /// Writes an i16 without checking overflow (caller must ensure space)
    fn write_i16_unchecked(&mut self, value: i16);

    /// Writes a byte without checking overflow (caller must ensure space)
    fn write_byte_unchecked(&mut self, value: u8);

    /// Writes an i32 without checking overflow (caller must ensure space)
    fn write_i32_unchecked(&mut self, value: i32);

    /// Writes a u16 without checking overflow (caller must ensure space)
    fn write_u16_unchecked(&mut self, value: u16);

    /// Writes an i64 without checking overflow (caller must ensure space)
    fn write_i64_unchecked(&mut self, value: i64);

    /// Writes a f64 without checking overflow (caller must ensure space)
    fn write_f64_unchecked(&mut self, value: f64);

    /// Checks if there's enough space for n bytes in the current packet.
    /// Returns true if space is available, false otherwise.
    ///
    /// If true: caller can use write_*_unchecked methods
    /// If false: caller should use async write APIs
    fn has_space(&self, bytes: usize) -> bool;

    /// Manually check and handle overflow after a batch of unchecked writes
    async fn check_overflow(&mut self) -> TdsResult<()>;
}

#[async_trait]
pub(crate) trait TdsPacketWriter {
    /// Writes a byte to the buffer.
    async fn write_byte_async(&mut self, value: u8) -> TdsResult<()>;

    /// Writes an i16 value in little-endian format.
    async fn write_i16_async(&mut self, value: i16) -> TdsResult<()>;

    /// Writes a u16 value in little-endian format.
    async fn write_u16_async(&mut self, value: u16) -> TdsResult<()>;

    /// Writes an i32 value in little-endian format.
    async fn write_i32_async(&mut self, value: i32) -> TdsResult<()>;

    /// Writes a u32 value in little-endian format.
    async fn write_u32_async(&mut self, value: u32) -> TdsResult<()>;

    /// Writes an i64 value in little-endian format.
    async fn write_i64_async(&mut self, value: i64) -> TdsResult<()>;

    /// Writes a u64 value in little-endian format.
    async fn write_u64_async(&mut self, value: u64) -> TdsResult<()>;

    /// Writes an i16 value in big-endian format.
    async fn write_i16_be_async(&mut self, value: i16) -> TdsResult<()>;

    /// Writes an i32 value in big-endian format.
    async fn write_i32_be_async(&mut self, value: i32) -> TdsResult<()>;

    /// Writes a string in Unicode (UTF-16LE) format.
    async fn write_string_unicode_async(&mut self, value: &str) -> TdsResult<()>;

    /// Writes raw bytes to the buffer.
    async fn write_async(&mut self, content: &[u8]) -> TdsResult<()>;

    /// Writes an i32 value at a specific index in the buffer.
    #[allow(dead_code)] // used in tests
    fn write_i32_at_index(&mut self, index: usize, value: i32);

    /// Finalizes the packet writer, sending any remaining data in the buffer.
    async fn finalize(&mut self) -> TdsResult<()>;
}

/// A packet writer that writes data to a buffer and if needed flushes it to the network as needed.
///
pub struct PacketWriter<'a> {
    packet_type: PacketType,
    network_writer: &'a mut dyn NetworkWriter,
    max_payload_size: usize,
    packet_id: u8,
    payload_cursor: Cursor<Vec<u8>>,
    packet_size: usize,
    is_first_packet: bool, // Note: Cannot just use packet_id because its value can rollover.
    start_time: Instant,
    max_timeout_sec: Option<u32>,
    cancel_handle: Option<CancelHandle>,
}

impl<'a> PacketWriter<'a> {
    pub(crate) const PACKET_HEADER_SIZE: usize = 8;

    pub(crate) fn new(
        packet_type: PacketType,
        network_writer: &'a mut dyn NetworkWriter,
        timeout: Option<u32>,
        cancel_handle: Option<&CancelHandle>,
    ) -> PacketWriter<'a> {
        let packet_size: usize = network_writer.packet_size() as usize;
        // Add additional space for the numeric types.
        let buffer: Vec<u8> = Vec::with_capacity(packet_size + size_of::<u64>());
        let mut buffer_cursor = Cursor::new(buffer);

        // Position the cursor at the end of the header. The header will be populated later.
        buffer_cursor.set_position(Self::PACKET_HEADER_SIZE as u64);

        // Normalise 0 → None (infinite timeout)
        let effective_timeout = timeout.filter(|&t| t > 0);

        PacketWriter {
            packet_type,
            network_writer,
            max_payload_size: packet_size - (Self::PACKET_HEADER_SIZE),
            packet_id: 1,
            payload_cursor: buffer_cursor,
            packet_size,
            is_first_packet: true,
            start_time: Instant::now(),
            max_timeout_sec: effective_timeout,
            cancel_handle: cancel_handle.map(|handle| handle.child_handle()),
        }
    }

    #[cfg(test)]
    pub(crate) async fn cancel_current_message(&mut self) -> TdsResult<()> {
        self.populate_header_and_send(true, true).await
    }

    pub(crate) fn position(&self) -> i32 {
        (self.payload_cursor.position() - Self::PACKET_HEADER_SIZE as u64) as i32
    }

    async fn handle_overflow_if_needed(&mut self) -> TdsResult<()> {
        // If the payload size is greater than the max payload size, send the packet.
        if self.position() >= (self.max_payload_size as i32) {
            self.populate_header_and_send(false, false).await?;

            let current_position = self.payload_cursor.position();
            let overflow_length = current_position as usize - self.packet_size;

            // Copy from the overflow buffer to the beginning of the buffer and reset the cursor.
            let original_buffer = self.payload_cursor.get_mut();

            // We have written beyond the packet size, so we need to copy the overflow data to the beginning of the buffer to the packet start.
            original_buffer.copy_within(
                self.packet_size..self.packet_size + overflow_length,
                Self::PACKET_HEADER_SIZE,
            );
            // Position cursor at the end of the copied overflow data
            self.payload_cursor
                .set_position((Self::PACKET_HEADER_SIZE + overflow_length) as u64);
        }
        Ok(())
    }

    /// Builds and sends a packet based on the current payload and the state of the message.
    ///
    /// # Arguments
    ///
    /// * `is_last_packet` - Flag indicating that this is the last packet of the current message.
    /// * `is_ignore_packet` - Flag indicating that the current message should be ignored by the
    ///   server. If this flag is set to true, the `is_last_packet` flag also must be set to true
    ///   as specified by the TDS protocol.
    /// ```
    async fn populate_header_and_send(
        &mut self,
        is_last_packet: bool,
        is_ignore_packet: bool,
    ) -> TdsResult<()> {
        // If the ignore bit is set, it must be the end of the message per the protocol.
        assert!(is_last_packet || !is_ignore_packet);

        // Record the position of the packet payload. Set the payload size to zero if this is an ignore packet.
        let saved_position = match is_ignore_packet {
            true => 0,
            false => self.payload_cursor.position(),
        };

        let packet_length = match saved_position as usize > self.packet_size {
            true => self.packet_size,
            false => saved_position as usize,
        };

        // Position at the header start and start writing the header.
        self.payload_cursor.set_position(0);
        let _ = Self::build_header(
            &mut self.payload_cursor,
            packet_length,
            self.packet_type,
            self.packet_id,
            is_last_packet,
            is_ignore_packet,
        );
        let data_slice = &self.payload_cursor.get_ref().as_slice()[..packet_length];

        // Send the packet data. The write must complete fully — dropping a
        // write future mid-flight leaves the TLS stream (especially SChannel
        // on Windows) in an inconsistent internal state, which causes the
        // next write to panic (issue #513). The timeout is checked *after*
        // the write finishes so that the stream always remains in a clean
        // state and attention packets can be sent safely on timeout.
        let send_data_fut = CancelHandle::run_until_cancelled(
            self.cancel_handle.as_ref(),
            self.network_writer.send(data_slice),
        );

        send_data_fut.await?;

        // Check timeout after the write completes.
        if let Some(max_timeout) = self.max_timeout_sec {
            let elapsed = self.start_time.elapsed().as_secs();
            if elapsed > max_timeout as u64 {
                return Err(TimeoutError(TimeoutErrorType::String(
                    "Timeout expired".to_string(),
                )));
            }
        }

        event!(
            tracing::Level::DEBUG,
            "Sending packet of size: {:?}",
            packet_length
        );
        use pretty_hex::PrettyHex;
        event!(
            tracing::Level::DEBUG,
            "Packet content: {:?}",
            data_slice.hex_dump()
        );

        // Invoke the first-packet callback if needed.
        if self.is_first_packet {
            self.packet_type
                .first_packet_callback(self.network_writer)
                .await?;
            self.is_first_packet = false;
        }

        // Add the counter for the packet and increment by 1 for the next packet.
        self.packet_id = self.packet_id.wrapping_add(1);

        // Restore the cursor position.
        self.payload_cursor.set_position(saved_position);
        Ok(())
    }

    pub(crate) fn build_header<W: WriteBytesExt>(
        writer: &mut W,
        packet_length: usize,
        packet_type: PacketType,
        packet_id: u8,
        is_last_packet: bool,
        is_ignore_packet: bool,
    ) -> TdsResult<()> {
        let _ = WriteBytesExt::write_u8(writer, packet_type as u8);
        let status = match is_last_packet {
            true => match is_ignore_packet {
                true => PacketStatusFlags::Eom as u8 | PacketStatusFlags::Ignore as u8,
                false => PacketStatusFlags::Eom as u8,
            },
            false => PacketStatusFlags::Normal as u8,
        };

        let _ = WriteBytesExt::write_u8(writer, status);

        let _ = WriteBytesExt::write_u16::<BigEndian>(writer, packet_length as u16);

        let _ = WriteBytesExt::write_u16::<BigEndian>(writer, 0);

        let _ = WriteBytesExt::write_u8(writer, packet_id);
        Ok(WriteBytesExt::write_u8(writer, 0)?)
    }

    #[cfg(test)]
    pub(crate) fn get_cursor(&self) -> &Cursor<Vec<u8>> {
        &self.payload_cursor
    }
}

#[async_trait]
impl TdsPacketWriter for PacketWriter<'_> {
    async fn finalize(&mut self) -> TdsResult<()> {
        if (self.payload_cursor.position()) > Self::PACKET_HEADER_SIZE as u64 {
            self.populate_header_and_send(true, false).await?;
            self.payload_cursor
                .set_position(Self::PACKET_HEADER_SIZE as u64);
        }
        Ok(())
    }

    /// Writes a byte to the buffer.
    async fn write_byte_async(&mut self, value: u8) -> TdsResult<()> {
        let _ = WriteBytesExt::write_u8(&mut self.payload_cursor, value);
        self.handle_overflow_if_needed().await
    }

    async fn write_i16_async(&mut self, value: i16) -> TdsResult<()> {
        let _ =
            WriteBytesExt::write_i16::<byteorder::LittleEndian>(&mut self.payload_cursor, value);
        self.handle_overflow_if_needed().await
    }

    async fn write_u16_async(&mut self, value: u16) -> TdsResult<()> {
        let _ =
            WriteBytesExt::write_u16::<byteorder::LittleEndian>(&mut self.payload_cursor, value);
        self.handle_overflow_if_needed().await
    }

    async fn write_i32_async(&mut self, value: i32) -> TdsResult<()> {
        let _ =
            WriteBytesExt::write_i32::<byteorder::LittleEndian>(&mut self.payload_cursor, value);
        self.handle_overflow_if_needed().await
    }

    async fn write_u32_async(&mut self, value: u32) -> TdsResult<()> {
        let _ =
            WriteBytesExt::write_u32::<byteorder::LittleEndian>(&mut self.payload_cursor, value);
        self.handle_overflow_if_needed().await
    }

    async fn write_i64_async(&mut self, value: i64) -> TdsResult<()> {
        let _ =
            WriteBytesExt::write_i64::<byteorder::LittleEndian>(&mut self.payload_cursor, value);
        self.handle_overflow_if_needed().await
    }

    async fn write_u64_async(&mut self, value: u64) -> TdsResult<()> {
        let _ =
            WriteBytesExt::write_u64::<byteorder::LittleEndian>(&mut self.payload_cursor, value);
        self.handle_overflow_if_needed().await
    }

    async fn write_i16_be_async(&mut self, value: i16) -> TdsResult<()> {
        let _ = WriteBytesExt::write_i16::<BigEndian>(&mut self.payload_cursor, value);
        self.handle_overflow_if_needed().await
    }

    async fn write_i32_be_async(&mut self, value: i32) -> TdsResult<()> {
        let _ = WriteBytesExt::write_i32::<BigEndian>(&mut self.payload_cursor, value);
        self.handle_overflow_if_needed().await
    }

    async fn write_string_unicode_async(&mut self, value: &str) -> TdsResult<()> {
        // Streaming UTF-16LE encoding: encodes and writes directly to buffer without intermediate Vec allocation
        let mut utf16_iter = value.encode_utf16();

        loop {
            let packet_space_left = self.max_payload_size - self.position() as usize;

            // How many u16 units can we write? (each u16 = 2 bytes)
            let u16_units_available = packet_space_left / 2;

            if u16_units_available == 0 {
                // Check if we have exactly 1 byte left - we can write the low byte of next u16
                if packet_space_left == 1 {
                    if let Some(u16_char) = utf16_iter.next() {
                        // Write the low byte of the u16
                        self.write_byte_async(u16_char as u8).await?;
                        // After flush, write the high byte
                        self.write_byte_async((u16_char >> 8) as u8).await?;
                        continue;
                    } else {
                        // No more characters to write
                        return Ok(());
                    }
                }
                // No space left, flush and continue
                self.populate_header_and_send(false, false).await?;
                self.payload_cursor
                    .set_position(Self::PACKET_HEADER_SIZE as u64);
                continue;
            }

            // Write as many u16 units as we can fit
            let mut units_written = 0;
            for _ in 0..u16_units_available {
                if let Some(u16_char) = utf16_iter.next() {
                    // Write u16 in little-endian directly to buffer
                    self.write_u16_unchecked(u16_char);
                    units_written += 1;
                } else {
                    // Finished writing all characters
                    if units_written > 0 {
                        // Check for overflow after batch write
                        self.check_overflow().await?;
                    }
                    return Ok(());
                }
            }

            // We filled the available space, check overflow and loop to flush
            if units_written > 0 {
                self.check_overflow().await?;
            }
        }
    }

    async fn write_async(&mut self, content: &[u8]) -> TdsResult<()> {
        // Write in chunks of packet size using an iterative approach
        // to avoid stack overflow with large data.
        let mut remaining = content;

        while !remaining.is_empty() {
            let packet_space_left = self.max_payload_size - self.position() as usize;

            if packet_space_left < remaining.len() {
                // Fill the current packet and flush
                let chunk = &remaining[..packet_space_left];
                let _ = std::io::Write::write_all(&mut self.payload_cursor, chunk);
                self.populate_header_and_send(false, false).await?;
                self.payload_cursor
                    .set_position(Self::PACKET_HEADER_SIZE as u64);
                remaining = &remaining[packet_space_left..];
            } else {
                // All remaining data fits in current packet
                let _ = std::io::Write::write_all(&mut self.payload_cursor, remaining);
                break;
            }
        }
        Ok(())
    }

    fn write_i32_at_index(&mut self, index: usize, value: i32) {
        let position = self.payload_cursor.position();
        self.payload_cursor
            .set_position((Self::PACKET_HEADER_SIZE + index) as u64);
        let _ =
            WriteBytesExt::write_i32::<byteorder::LittleEndian>(&mut self.payload_cursor, value);
        self.payload_cursor.set_position(position);
    }
}

// Implement the unchecked write trait separately for optimized batch operations
impl TdsPacketWriterUnchecked for PacketWriter<'_> {
    fn write_byte_unchecked(&mut self, value: u8) {
        let _ = WriteBytesExt::write_u8(&mut self.payload_cursor, value);
    }

    fn write_i32_unchecked(&mut self, value: i32) {
        let _ =
            WriteBytesExt::write_i32::<byteorder::LittleEndian>(&mut self.payload_cursor, value);
    }

    fn write_u16_unchecked(&mut self, value: u16) {
        let _ =
            WriteBytesExt::write_u16::<byteorder::LittleEndian>(&mut self.payload_cursor, value);
    }

    fn write_i16_unchecked(&mut self, value: i16) {
        let _ =
            WriteBytesExt::write_i16::<byteorder::LittleEndian>(&mut self.payload_cursor, value);
    }

    fn write_i64_unchecked(&mut self, value: i64) {
        let _ =
            WriteBytesExt::write_i64::<byteorder::LittleEndian>(&mut self.payload_cursor, value);
    }

    fn write_f64_unchecked(&mut self, value: f64) {
        let _ =
            WriteBytesExt::write_f64::<byteorder::LittleEndian>(&mut self.payload_cursor, value);
    }

    fn has_space(&self, bytes_count: usize) -> bool {
        // Check if there's space left in the current packet
        let current_pos = self.position() as usize;
        current_pos + bytes_count <= self.max_payload_size
    }

    async fn check_overflow(&mut self) -> TdsResult<()> {
        self.handle_overflow_if_needed().await
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::vec;

    use super::*;
    use crate::connection::transport::network_transport::TransportSslHandler;
    use crate::core::NegotiatedEncryptionSetting;
    use async_trait::async_trait;
    use futures::executor::block_on;

    // Expose copy of internal buffer in PacketWriter for tests in other modules.
    impl PacketWriter<'_> {
        pub(crate) fn get_payload(&self) -> Cursor<Vec<u8>> {
            self.payload_cursor.clone()
        }
    }

    pub(crate) struct MockNetworkWriter {
        pub(crate) size: u32,
        pub(crate) data: Vec<u8>,
    }

    impl MockNetworkWriter {
        pub(crate) fn new(size: u32) -> Self {
            Self { size, data: vec![] }
        }
    }

    #[async_trait]
    impl NetworkWriter for MockNetworkWriter {
        #[allow(clippy::type_complexity, clippy::type_repetition_in_bounds)]
        async fn send(&mut self, _data: &[u8]) -> TdsResult<()> {
            // No op
            self.data.extend_from_slice(_data);
            Ok(())
        }

        fn packet_size(&self) -> u32 {
            self.size
        }

        fn get_encryption_setting(&self) -> NegotiatedEncryptionSetting {
            unimplemented!()
        }
    }

    #[async_trait]
    impl TransportSslHandler for MockNetworkWriter {
        async fn enable_ssl(&mut self) -> TdsResult<()> {
            unimplemented!()
        }

        async fn disable_ssl(&mut self) -> TdsResult<()> {
            unimplemented!()
        }
    }

    #[test]
    fn test_write_byte_async() {
        let mut mock = MockNetworkWriter::new(8);
        let mut writer = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        block_on(writer.write_byte_async(0xAB)).unwrap();
        assert_eq!(writer.payload_cursor.into_inner()[8..], vec![0xAB]);
    }

    #[test]
    fn test_write_i16_async() {
        let mut mock = MockNetworkWriter::new(8);
        let mut writer = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        block_on(writer.write_i16_async(0x1234)).unwrap();
        assert_eq!(
            writer.payload_cursor.into_inner()[8..],
            0x1234i16.to_le_bytes()
        );
    }

    #[test]
    fn test_write_u32_async() {
        let mut mock = MockNetworkWriter::new(8);
        let mut writer = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        block_on(TdsPacketWriter::write_u32_async(&mut writer, 0xDEADBEEF)).unwrap();
        assert_eq!(
            writer.payload_cursor.into_inner()[8..],
            0xDEADBEEFu32.to_le_bytes()
        );
    }

    #[test]
    fn test_write_i64_async() {
        let mut mock = MockNetworkWriter::new(16);
        let mut writer = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        block_on(TdsPacketWriter::write_i64_async(
            &mut writer,
            0x1122334455667788,
        ))
        .unwrap();
        assert_eq!(
            writer.payload_cursor.into_inner()[8..],
            0x1122334455667788i64.to_le_bytes()
        );
    }

    #[test]
    fn test_write_i64_overflow_async() {
        let mut mock = MockNetworkWriter::new(16);
        let mut writer = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        block_on(TdsPacketWriter::write_i32_async(&mut writer, 0x1234)).unwrap();
        block_on(TdsPacketWriter::write_i64_async(
            &mut writer,
            0x1122334455667788,
        ))
        .unwrap();
        assert_eq!(mock.data[8..12], 0x1234i32.to_le_bytes());
    }

    #[test]
    fn test_finalize_with_data() {
        let mut mock = MockNetworkWriter::new(16);
        let mut writer = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        block_on(writer.write_byte_async(0xAB)).unwrap();
        block_on(writer.finalize()).unwrap();
        assert_eq!(
            writer.payload_cursor.position(),
            PacketWriter::PACKET_HEADER_SIZE as u64
        );
        assert_eq!(writer.packet_id, 2);
    }

    #[test]
    fn test_finalize_without_data() {
        let mut mock = MockNetworkWriter::new(16);
        let mut writer = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        block_on(writer.finalize()).unwrap();
        assert_eq!(
            writer.payload_cursor.position(),
            PacketWriter::PACKET_HEADER_SIZE as u64
        );
        assert_eq!(writer.packet_id, 1);
    }

    #[test]
    fn test_get_cursor_returns_payload_cursor() {
        let mut mock = MockNetworkWriter::new(16);
        let mut writer = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);

        block_on(writer.write_byte_async(0xAB)).unwrap();

        assert_eq!(
            writer.get_cursor().position(),
            (PacketWriter::PACKET_HEADER_SIZE + 1) as u64
        );
    }

    #[test]
    fn test_cancel_current_message_sends_ignore_packet() {
        let mut mock = MockNetworkWriter::new(16);
        let mut writer = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);

        block_on(writer.write_byte_async(0xAB)).unwrap();
        block_on(writer.cancel_current_message()).unwrap();

        // Verify the header was written into the internal cursor with the Eom|Ignore flags.
        let buf = writer.payload_cursor.get_ref();
        assert_eq!(
            buf[1],
            PacketStatusFlags::Eom as u8 | PacketStatusFlags::Ignore as u8
        );
    }

    #[test]
    fn test_write_at_index() {
        let mut mock = MockNetworkWriter::new(16);
        let mut writer = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);

        block_on(writer.write_byte_async(0xAB)).unwrap();
        block_on(writer.write_byte_async(0xAB)).unwrap();
        block_on(writer.write_byte_async(0xAB)).unwrap();
        block_on(writer.write_byte_async(0xAB)).unwrap();
        block_on(writer.write_byte_async(0xAB)).unwrap();
        block_on(writer.write_byte_async(0xAB)).unwrap();
        block_on(writer.write_byte_async(0xAB)).unwrap();
        let value: i32 = 1234;
        assert_eq!(
            writer.payload_cursor.clone().into_inner()[8..12],
            [0xAB, 0xAB, 0xAB, 0xAB]
        );
        writer.write_i32_at_index(0, value);
        assert_eq!(
            writer.payload_cursor.into_inner()[8..12],
            value.to_le_bytes()
        );
    }

    #[test]
    fn test_write_string_overflow() {
        let packet_size: usize = 16;
        let mut mock = MockNetworkWriter::new(packet_size as u32);
        let mut writer = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        let str_value = "a very very very very very very very very very very very very long string";
        block_on(writer.write_string_unicode_async(str_value)).unwrap();
        block_on(writer.finalize()).unwrap();

        let mut string_vec: Vec<u8> = Vec::new();
        let data = mock.data;
        let mut chunks = data.len() / packet_size;
        if !data.len().is_multiple_of(packet_size) {
            chunks += 1;
        }
        for i in 0..chunks {
            let start = i * packet_size;
            let mut end = start + packet_size;
            if end > data.len() {
                end = data.len();
            }
            string_vec.extend_from_slice(&data[start + 8..end]);
        }

        let utf16_units = string_vec
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes(chunk.try_into().unwrap()))
            .collect::<Vec<u16>>();

        // get the utf18 value from string_vec
        let utf16_value = String::from_utf16(&utf16_units).unwrap();

        assert_eq!(utf16_value, str_value);
    }

    #[test]
    fn test_has_space_available() {
        let mut mock = MockNetworkWriter::new(32);
        let writer = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);

        // With packet size 32 and header 8, we have 24 bytes available
        // Should have space for 8 bytes
        assert!(writer.has_space(8), "Should have space for 8 bytes");
    }

    #[test]
    fn test_has_space_within_packet() {
        let mut mock = MockNetworkWriter::new(16);
        let mut writer = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);

        // Write some data first
        block_on(writer.write_i32_async(0x1234)).unwrap();

        // Now we have 4 bytes remaining in the 8-byte payload.
        // Asking for 4 bytes should return true
        assert!(writer.has_space(4), "Should have space for 4 bytes");
    }

    #[test]
    fn test_has_space_exceeds_packet() {
        let mut mock = MockNetworkWriter::new(16);
        let mut writer = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);

        // Fill buffer partially (packet size 16, header 8 = 8 bytes available)
        block_on(writer.write_i32_async(0x1234)).unwrap();

        // 4 bytes used, 4 remaining. Asking for 8 bytes exceeds packet boundary
        assert!(
            !writer.has_space(8),
            "Should not have space for 8 bytes (would exceed packet)"
        );
    }

    #[test]
    fn test_write_byte_unchecked() {
        let mut mock = MockNetworkWriter::new(16);
        let mut writer = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);

        writer.write_byte_unchecked(0xAB);
        writer.write_byte_unchecked(0xCD);

        assert_eq!(
            writer.payload_cursor.clone().into_inner()[8..10],
            [0xAB, 0xCD]
        );
    }

    #[test]
    fn test_write_i32_unchecked() {
        let mut mock = MockNetworkWriter::new(16);
        let mut writer = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);

        writer.write_i32_unchecked(0x12345678);

        assert_eq!(
            writer.payload_cursor.clone().into_inner()[8..12],
            0x12345678i32.to_le_bytes()
        );
    }

    #[test]
    fn test_write_u16_unchecked() {
        let mut mock = MockNetworkWriter::new(16);
        let mut writer = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);

        writer.write_u16_unchecked(0xABCD);

        assert_eq!(
            writer.payload_cursor.clone().into_inner()[8..10],
            0xABCDu16.to_le_bytes()
        );
    }

    #[test]
    fn test_write_i64_unchecked() {
        let mut mock = MockNetworkWriter::new(24);
        let mut writer = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);

        writer.write_i64_unchecked(0x123456789ABCDEF0);

        assert_eq!(
            writer.payload_cursor.clone().into_inner()[8..16],
            0x123456789ABCDEF0i64.to_le_bytes()
        );
    }

    #[test]
    fn test_write_f64_unchecked() {
        let mut mock = MockNetworkWriter::new(24);
        let mut writer = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);

        let value = 3.12312312312312;
        writer.write_f64_unchecked(value);

        assert_eq!(
            writer.payload_cursor.clone().into_inner()[8..16],
            value.to_le_bytes()
        );
    }

    #[test]
    fn test_unchecked_batch_writes() {
        let mut mock = MockNetworkWriter::new(32);
        let mut writer = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);

        // Batch write using unchecked methods
        writer.write_byte_unchecked(0x01);
        writer.write_u16_unchecked(0x0203);
        writer.write_i32_unchecked(0x04050607);
        writer.write_i64_unchecked(0x08090A0B0C0D0E0F);

        let buffer = writer.payload_cursor.clone().into_inner();
        assert_eq!(buffer[8], 0x01);
        assert_eq!(buffer[9..11], 0x0203u16.to_le_bytes());
        assert_eq!(buffer[11..15], 0x04050607i32.to_le_bytes());
        assert_eq!(buffer[15..23], 0x08090A0B0C0D0E0Fi64.to_le_bytes());
    }

    #[test]
    fn test_check_overflow_manual() {
        let mut mock = MockNetworkWriter::new(16);
        let mut writer = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);

        // Fill to capacity
        block_on(writer.write_i32_async(0x1234)).unwrap();
        block_on(writer.write_i32_async(0x5678)).unwrap();

        // Manual overflow check
        block_on(writer.check_overflow()).unwrap();

        // Now write more data
        writer.write_i32_unchecked(0x9ABC);

        // Verify the i32 was written correctly
        assert_eq!(
            writer.payload_cursor.clone().into_inner()[8..12],
            0x9ABCi32.to_le_bytes()
        );

        // Drop writer to release borrow of mock, then verify data was sent
        drop(writer);
        assert!(!mock.data.is_empty());
    }

    #[test]
    fn test_cursor_position_after_overflow() {
        let mut mock = MockNetworkWriter::new(20); // Header 8 + payload 12
        let mut writer = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);

        // Write data (10 bytes) that is less than payload capacity (12 bytes)
        block_on(writer.write_i16_async(0x1234)).unwrap(); // 2 bytes

        // This i64 write (8 bytes) will cause overflow:
        // Total would be 10 bytes, which exceeds 12 byte capacity
        // After write, cursor at 18 (header 8 + 10 bytes)
        // handle_overflow triggers:
        // 1. position() = 10, which is < 12, so no overflow... wait this won't trigger!

        // Let me use a case that actually overflows
        block_on(writer.write_i32_async(0x5678)).unwrap(); // 4 more bytes, total 6
        block_on(writer.write_i32_async(0x9ABC)).unwrap(); // 4 more bytes, total 10

        // Now write i64 (8 bytes). Total would be 18 bytes.
        // position() after i64 write = 18, max_payload = 12
        // This triggers overflow
        block_on(TdsPacketWriter::write_i64_async(
            &mut writer,
            0x123456789ABCDEF0,
        ))
        .unwrap();

        // After overflow:
        // 1. First packet sent (header 8 + first 12 bytes of payload)
        // 2. Overflow = 18 - 20 = -2? No wait, cursor is at 26 (8 header + 18 payload)
        //    packet_size = 20, so overflow_length = 26 - 20 = 6
        // 3. Copies 6 bytes from position 20-26 to position 8-14
        // 4. Sets cursor to 8 + 6 = 14
        assert_eq!(
            writer.payload_cursor.position(),
            (PacketWriter::PACKET_HEADER_SIZE + 6) as u64
        );
    }

    #[test]
    fn test_streaming_utf16_write() {
        let mut mock = MockNetworkWriter::new(64);
        let mut writer = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);

        let test_string = "Hello";
        block_on(writer.write_string_unicode_async(test_string)).unwrap();

        // Verify UTF-16LE encoding
        let buffer = writer.payload_cursor.clone().into_inner();
        let utf16_bytes = &buffer[8..18]; // "Hello" = 5 chars * 2 bytes = 10 bytes

        let utf16_units: Vec<u16> = utf16_bytes
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes(chunk.try_into().unwrap()))
            .collect();

        let decoded = String::from_utf16(&utf16_units).unwrap();
        assert_eq!(decoded, test_string);
    }

    #[test]
    fn test_streaming_utf16_odd_byte_boundary() {
        // Test case: packet with odd payload size (21 bytes available)
        // Packet size 29 = 8 (header) + 21 (payload)
        let packet_size = 29;
        let mut mock = MockNetworkWriter::new(packet_size);
        let mut writer = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);

        // String with 12 characters = 24 bytes in UTF-16LE
        // With 21 bytes available: can fit 10 units (20 bytes) + 1 byte of next unit
        // Optimization: use the odd byte for the low byte of the 11th u16
        let test_string = "HelloWorld12";
        block_on(writer.write_string_unicode_async(test_string)).unwrap();
        block_on(writer.finalize()).unwrap();

        // Reconstruct the string from all packets
        let mut string_vec: Vec<u8> = Vec::new();
        let data = &mock.data;

        // Parse packets - each packet starts with header
        let mut offset = 0;
        let mut first_packet_payload_len = 0;
        while offset < data.len() {
            if offset + 4 > data.len() {
                break;
            }
            let packet_len = u16::from_be_bytes([data[offset + 2], data[offset + 3]]) as usize;
            if offset + packet_len > data.len() {
                break;
            }
            // Extract payload (skip 8-byte header)
            let payload_len = packet_len - 8;
            if first_packet_payload_len == 0 {
                first_packet_payload_len = payload_len;
            }
            string_vec.extend_from_slice(&data[offset + 8..offset + packet_len]);
            offset += packet_len;
        }

        let utf16_units: Vec<u16> = string_vec
            .chunks_exact(2)
            .map(|chunk| u16::from_le_bytes(chunk.try_into().unwrap()))
            .collect();

        let decoded = String::from_utf16(&utf16_units).unwrap();
        assert_eq!(decoded, test_string);

        // Verify we sent multiple packets (string doesn't fit in one packet)
        assert!(
            mock.data.len() > packet_size as usize,
            "Should have sent multiple packets"
        );

        // After optimization: should use all 21 bytes (10 u16 units + 1 byte)
        assert_eq!(
            first_packet_payload_len, 21,
            "Should use all 21 bytes including the odd byte"
        );
    }

    /// Test that write_async handles very large data without stack overflow.
    /// This test reproduces issue #41685 where a 50MB+ string caused a segfault
    /// due to deep recursion in write_async (943+ recursive calls).
    ///
    /// The fix converts write_async from a recursive approach to an iterative one.
    #[test]
    fn test_write_async_large_data_no_stack_overflow() {
        // Use a realistic packet size (4096 bytes, so 4088 bytes payload)
        let packet_size: usize = 4096;
        let mut mock = MockNetworkWriter::new(packet_size as u32);
        let mut writer = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);

        // Create a large byte array that would require many packets
        // 10MB = 10485760 bytes, with 4088 byte payload = ~2565 packets/recursive calls
        // With the OLD recursive approach, this causes stack overflow
        // With the NEW iterative approach, this works fine
        let data_size = 50 * 1024 * 1024; // 10MB - definitely causes stack overflow with recursion
        let large_data: Vec<u8> = (0..data_size).map(|i| (i % 256) as u8).collect();

        block_on(writer.write_async(&large_data)).unwrap();
        block_on(writer.finalize()).unwrap();

        // Reconstruct data from packets
        let mut reconstructed: Vec<u8> = Vec::new();
        let data = &mock.data;
        let mut offset = 0;

        while offset < data.len() {
            if offset + 4 > data.len() {
                break;
            }
            let packet_len = u16::from_be_bytes([data[offset + 2], data[offset + 3]]) as usize;
            if offset + packet_len > data.len() {
                break;
            }
            // Extract payload (skip 8-byte header)
            reconstructed.extend_from_slice(&data[offset + 8..offset + packet_len]);
            offset += packet_len;
        }

        assert_eq!(reconstructed.len(), large_data.len());
        assert_eq!(reconstructed, large_data);
    }

    /// Test write_async with data that spans exactly the packet boundary
    #[test]
    fn test_write_async_exact_packet_boundary() {
        let packet_size: usize = 16; // 8 bytes payload
        let mut mock = MockNetworkWriter::new(packet_size as u32);
        let mut writer = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);

        // Write exactly 8 bytes (fills one packet payload)
        let data = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08];
        block_on(writer.write_async(&data)).unwrap();
        block_on(writer.finalize()).unwrap();

        // Should have sent one complete packet
        assert_eq!(mock.data.len(), packet_size);
        assert_eq!(&mock.data[8..16], &data);
    }

    /// Test write_async with data spanning multiple packets
    #[test]
    fn test_write_async_multiple_packets() {
        let packet_size: usize = 16; // 8 bytes payload
        let mut mock = MockNetworkWriter::new(packet_size as u32);
        let mut writer = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);

        // Write 24 bytes (needs 3 packets with 8-byte payload each)
        let data: Vec<u8> = (0..24).collect();
        block_on(writer.write_async(&data)).unwrap();
        block_on(writer.finalize()).unwrap();

        // Reconstruct and verify
        let mut reconstructed: Vec<u8> = Vec::new();
        let sent = &mock.data;
        let mut offset = 0;

        while offset < sent.len() {
            let packet_len = u16::from_be_bytes([sent[offset + 2], sent[offset + 3]]) as usize;
            reconstructed.extend_from_slice(&sent[offset + 8..offset + packet_len]);
            offset += packet_len;
        }

        assert_eq!(reconstructed, data);
    }

    #[test]
    fn test_zero_timeout_treated_as_infinite() {
        let packet_size: usize = 4096;
        let mut mock = MockNetworkWriter::new(packet_size as u32);
        let writer = PacketWriter::new(PacketType::TabularResult, &mut mock, Some(0), None);
        assert!(
            writer.max_timeout_sec.is_none(),
            "timeout=0 should be normalised to None (infinite)"
        );
    }

    #[test]
    fn test_positive_timeout_preserved() {
        let packet_size: usize = 4096;
        let mut mock = MockNetworkWriter::new(packet_size as u32);
        let writer = PacketWriter::new(PacketType::TabularResult, &mut mock, Some(30), None);
        assert_eq!(writer.max_timeout_sec, Some(30));
    }

    #[test]
    fn test_none_timeout_stays_none() {
        let packet_size: usize = 4096;
        let mut mock = MockNetworkWriter::new(packet_size as u32);
        let writer = PacketWriter::new(PacketType::TabularResult, &mut mock, None, None);
        assert!(writer.max_timeout_sec.is_none());
    }
}
